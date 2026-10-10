//! Framework-free funnel handlers (port of two-bot `src/core/handlers.ts`).
//!
//! Nothing here imports a Discord type: the twilight pipeline in
//! `two-bot-discord` turns gateway dispatches into these plain calls, so the
//! funnel rules are testable without a network, a token, or a server.
//!
//! Storage lives behind [`FunnelStore`] (S6 implements it with sqlx;
//! tests use [`MemStore`]). Leveling awards and community-facts capture are
//! optional seams ([`LevelingHook`], [`FactsSink`]) owned by S4/S5 — the call
//! sites and ordering are ported faithfully here so those slices plug in
//! without touching the funnel rules.
//!
//! All handler methods take `&self`: the open voice map sits behind a mutex
//! so the shared async pipeline (per-member serial chains, TOG-5981) can hold
//! one `Arc<FunnelHandlers>` while voice joins mutate tracker state.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::{
    funnel::{idempotency_key, now_iso, EventType, FunnelEvent, MESSAGE_RUNGS},
    voice::{resolve_voice_end, VoiceEnd, VoiceSessionTracker},
    Snowflake,
};

// --- store seam ---------------------------------------------------------------

/// What `record` reports: whether the row was new (idempotency-key winner).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordOutcome {
    pub inserted: bool,
}

/// One persisted funnel row (the `events` table shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRow {
    pub guild_id: Snowflake,
    pub member_id: Option<Snowflake>,
    pub event_type: EventType,
    pub occurred_at: String,
    pub source: String,
    pub metadata: Option<serde_json::Value>,
    pub dedupe_token: Option<String>,
    pub idempotency_key: String,
}

impl From<&FunnelEvent> for StoredRow {
    fn from(e: &FunnelEvent) -> Self {
        Self {
            guild_id: e.guild_id,
            member_id: e.member_id,
            event_type: e.event_type,
            occurred_at: e.occurred_at.clone(),
            source: e.source.clone(),
            metadata: e.metadata.clone(),
            dedupe_token: e.dedupe_token.clone(),
            idempotency_key: idempotency_key(e),
        }
    }
}

/// Event-log persistence + the two read models the handlers need.
/// S6 implements this over Postgres (`ON CONFLICT DO NOTHING` arbitrates the
/// idempotency key atomically); [`MemStore`] is the test double.
pub trait FunnelStore: Send + Sync {
    /// Persist an observed bot before any unconditional funnel projection.
    /// Classification is monotonic: missing user data never demotes a bot.
    fn mark_bot(&self, guild_id: Snowflake, member_id: Snowflake);
    /// Insert-or-ignore by idempotency key.
    fn record(&self, event: FunnelEvent) -> RecordOutcome;
    /// Monotonic recency bump (never moves backwards).
    fn touch_activity(&self, guild_id: Snowflake, member_id: Snowflake, at: &str);
    /// Lowest empty message rung for a message arriving at `at`, or `None`
    /// when the ladder is full — including the redelivery guard (a rung is
    /// filled only strictly after the rung below).
    fn next_message_rung(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        at: &str,
    ) -> Option<EventType>;
    fn has_event(&self, guild_id: Snowflake, member_id: Snowflake, event_type: EventType) -> bool;
    /// Optional durable invite seam; in-memory replay already owns its tracker.
    fn stage_invite_snapshot(&self, _snapshot: crate::gateway_funnel::InviteSnapshotWrite) {}
}

/// In-memory [`FunnelStore`] for tests and the replay harness.
#[derive(Debug, Default)]
pub struct MemStore {
    inner: Mutex<MemInner>,
}

#[derive(Debug, Default)]
struct MemInner {
    rows: Vec<StoredRow>,
    keys: std::collections::HashSet<String>,
    activity: std::collections::HashMap<(Snowflake, Snowflake), String>,
    bots: std::collections::HashSet<(Snowflake, Snowflake)>,
}

impl MemStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// All rows in insert order.
    pub fn rows(&self) -> Vec<StoredRow> {
        self.inner.lock().expect("store lock").rows.clone()
    }

    /// Last-activity per member.
    pub fn activity(&self, guild_id: Snowflake, member_id: Snowflake) -> Option<String> {
        self.inner
            .lock()
            .expect("store lock")
            .activity
            .get(&(guild_id, member_id))
            .cloned()
    }
}

impl FunnelStore for MemStore {
    fn mark_bot(&self, guild_id: Snowflake, member_id: Snowflake) {
        self.inner
            .lock()
            .expect("store lock")
            .bots
            .insert((guild_id, member_id));
    }

    fn record(&self, event: FunnelEvent) -> RecordOutcome {
        crate::membership::MembershipStore::record_observed(self, event, None)
    }

    fn touch_activity(&self, guild_id: Snowflake, member_id: Snowflake, at: &str) {
        let mut inner = self.inner.lock().expect("store lock");
        let entry = inner.activity.entry((guild_id, member_id)).or_default();
        if entry.is_empty() || entry.as_str() < at {
            *entry = at.to_owned();
        }
    }

    fn next_message_rung(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        at: &str,
    ) -> Option<EventType> {
        let inner = self.inner.lock().expect("store lock");
        let mut filled: std::collections::HashMap<EventType, &str> =
            std::collections::HashMap::new();
        for row in &inner.rows {
            if row.guild_id == guild_id
                && row.member_id == Some(member_id)
                && MESSAGE_RUNGS.contains(&row.event_type)
            {
                filled
                    .entry(row.event_type)
                    .or_insert(row.occurred_at.as_str());
            }
        }
        let mut below: Option<&str> = None;
        for rung in MESSAGE_RUNGS {
            match filled.get(&rung) {
                None => {
                    // Redelivery guard: same instant as the rung below is the
                    // same message, not a new one.
                    if below.is_some_and(|b| at <= b) {
                        return None;
                    }
                    return Some(rung);
                }
                Some(stamp) => below = Some(stamp),
            }
        }
        None
    }

    fn has_event(&self, guild_id: Snowflake, member_id: Snowflake, event_type: EventType) -> bool {
        self.inner
            .lock()
            .expect("store lock")
            .rows
            .iter()
            .any(|row| {
                row.guild_id == guild_id
                    && row.member_id == Some(member_id)
                    && row.event_type == event_type
            })
    }
}

impl crate::membership::MembershipStore for MemStore {
    fn membership(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
    ) -> Option<crate::membership::Membership> {
        let inner = self.inner.lock().expect("store lock");
        crate::membership::project(
            inner
                .rows
                .iter()
                .filter(|r| r.guild_id == guild_id && r.member_id == Some(member_id)),
        )
    }

    fn membership_rows(&self, guild_id: Snowflake, member_id: Snowflake) -> Vec<StoredRow> {
        self.rows()
            .into_iter()
            .filter(|r| r.guild_id == guild_id && r.member_id == Some(member_id))
            .collect()
    }

    fn record_observed(&self, event: FunnelEvent, observed_at: Option<&str>) -> RecordOutcome {
        let mut row = StoredRow::from(&event);
        let mut inner = self.inner.lock().expect("store lock");
        if inner.keys.contains(&row.idempotency_key) {
            if let Some(hint) = observed_at {
                let existing = inner
                    .rows
                    .iter_mut()
                    .find(|r| r.idempotency_key == row.idempotency_key)
                    .expect("key has a row");
                crate::membership::advance_observation(existing, hint);
            }
            return RecordOutcome { inserted: false };
        }
        if let Some(hint) = observed_at {
            crate::membership::set_observation(&mut row, hint);
        }
        inner.keys.insert(row.idempotency_key.clone());
        inner.rows.push(row);
        RecordOutcome { inserted: true }
    }
}

// --- collaborator seams (S4/S5 own the implementations) ------------------------

/// Leveling award outcome: whether a level was crossed and the new level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelOutcome {
    pub leveled_up: bool,
    pub level: u64,
}

/// XP awards (S4 leveling + S6 cooldown storage). The call sites and ordering
/// are fixed here; this trait decides the award.
pub trait LevelingHook: Send + Sync {
    fn award_message(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        at: &str,
        channel_id: Snowflake,
    ) -> LevelOutcome;
    fn award_voice(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        duration_seconds: u64,
        at: &str,
        channel_id: Snowflake,
    ) -> LevelOutcome;
}

/// No-op leveling (S3 default; S4/S6 plug in the real awards).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopLeveling;

impl LevelingHook for NoopLeveling {
    fn award_message(
        &self,
        _guild_id: Snowflake,
        _member_id: Snowflake,
        _at: &str,
        _channel_id: Snowflake,
    ) -> LevelOutcome {
        LevelOutcome {
            leveled_up: false,
            level: 0,
        }
    }

    fn award_voice(
        &self,
        _guild_id: Snowflake,
        _member_id: Snowflake,
        _duration_seconds: u64,
        _at: &str,
        _channel_id: Snowflake,
    ) -> LevelOutcome {
        LevelOutcome {
            leveled_up: false,
            level: 0,
        }
    }
}

/// Community channel class for message attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelClass {
    Welcome,
    Human,
    #[default]
    Other,
}

impl ChannelClass {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Welcome => "welcome",
            Self::Human => "human",
            Self::Other => "other",
        }
    }
}

/// Member-join fact (legacy `recordMemberJoin` fields, bundled so the sink
/// seam stays under the argument limit and S5 call sites are named).
#[derive(Debug, Clone)]
pub struct MemberJoinFact<'a> {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub is_bot: bool,
    pub occurred_at: &'a str,
    pub source_event_id: &'a str,
    pub source: &'a str,
    pub inviter_id: Option<Snowflake>,
}

/// Rules-accepted fact (legacy `recordRulesAccepted`).
#[derive(Debug, Clone)]
pub struct RulesAcceptedFact<'a> {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub is_bot: bool,
    pub occurred_at: &'a str,
    pub source_event_id: String,
    pub source: &'a str,
}

/// Message fact (legacy `recordMessage`).
#[derive(Debug, Clone)]
pub struct MessageFact<'a> {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub is_bot: bool,
    pub webhook_id: Option<Snowflake>,
    pub is_staff_automation: bool,
    pub message_id: &'a str,
    pub channel_id: Snowflake,
    pub channel_class: ChannelClass,
    pub occurred_at: &'a str,
}

/// Voice-start fact (legacy `recordVoiceStarted`).
#[derive(Debug, Clone)]
pub struct VoiceStartedFact<'a> {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub is_bot: bool,
    pub channel_id: Snowflake,
    pub occurred_at: &'a str,
}

/// Voice-end fact (legacy `recordVoiceEnded`).
#[derive(Debug, Clone)]
pub struct VoiceEndedFact<'a> {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub is_bot: bool,
    pub session_key: String,
    pub channel_id: Snowflake,
    pub occurred_at: &'a str,
    pub started_at: Option<&'a str>,
    pub duration_seconds: Option<i64>,
}

/// Community-facts capture (S5 scorecard). `record_voice_started` returns the
/// durable scorecard key for the session, like legacy `recordVoiceStarted`.
pub trait FactsSink: Send + Sync {
    fn record_member_join(&self, fact: MemberJoinFact<'_>);
    fn record_rules_accepted(&self, fact: RulesAcceptedFact<'_>);
    fn record_message(&self, fact: MessageFact<'_>);
    fn record_voice_started(&self, fact: VoiceStartedFact<'_>) -> Option<String>;
    fn record_voice_ended(&self, fact: VoiceEndedFact<'_>);
}

/// No-op facts sink (S3 default; S5 plugs in the scorecard capture).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopFacts;

impl FactsSink for NoopFacts {
    fn record_member_join(&self, _fact: MemberJoinFact<'_>) {}

    fn record_rules_accepted(&self, _fact: RulesAcceptedFact<'_>) {}

    fn record_message(&self, _fact: MessageFact<'_>) {}

    fn record_voice_started(&self, _fact: VoiceStartedFact<'_>) -> Option<String> {
        None
    }

    fn record_voice_ended(&self, _fact: VoiceEndedFact<'_>) {}
}

// --- inputs --------------------------------------------------------------------

/// Join attribution input (invite tracker / expected-joins resolved upstream).
#[derive(Debug, Clone)]
pub struct JoinInput {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub is_bot: bool,
    pub source: String,
    pub occurred_at: Option<String>,
    pub inviter_id: Option<Snowflake>,
    pub source_event_id: Option<String>,
}

/// Rules-gate clearing. The gateway gives no timestamp for the transition,
/// so the live path leaves `occurred_at` unset (becomes now, accurate to the
/// second). A backfill MUST pass an explicit time + `backfill:*` source.
#[derive(Debug, Clone)]
pub struct GateClearedInput {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub is_bot: bool,
    pub occurred_at: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MessageInput {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub is_bot: bool,
    pub message_id: Option<String>,
    pub webhook_id: Option<Snowflake>,
    pub is_staff_automation: bool,
    pub channel_id: Snowflake,
    pub channel_class: ChannelClass,
    pub capture_only: bool,
    pub occurred_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VoiceInput {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub is_bot: bool,
    pub channel_id: Snowflake,
    pub occurred_at: Option<String>,
}

/// `voice_session_end` metadata blob. Assembled by [`voice_end_metadata_json`]
/// in legacy `JSON.stringify({startKnown, startedAt, durationSeconds})` key
/// order, preserved through `serde_json::Value` by the workspace
/// `preserve_order` feature. Scalar values still serialize via serde
/// (deterministic); only the object assembly is manual.
///
/// What one handler call produced: the funnel event (legacy return shape —
/// `on_voice_join` still returns the `first_voice_session` event or null) plus
/// the level the member crossed, if any (role application is S4's job).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerOutcome {
    pub event: Option<FunnelEvent>,
    pub leveled_up_to: Option<u64>,
}

impl HandlerOutcome {
    fn event(event: FunnelEvent) -> Self {
        Self {
            event: Some(event),
            leveled_up_to: None,
        }
    }

    fn none() -> Self {
        Self {
            event: None,
            leveled_up_to: None,
        }
    }
}

// --- handlers -------------------------------------------------------------------

/// Render a `voice_session_end` metadata blob in legacy byte order
/// (`startKnown`, `startedAt`, `durationSeconds`). Values serialize via serde;
/// the object assembly is manual and key order survives through the workspace
/// `preserve_order` feature.
#[must_use]
pub fn voice_end_metadata_json(
    start_known: bool,
    started_at: Option<&str>,
    duration_seconds: Option<i64>,
) -> serde_json::Value {
    let started_at_json = started_at.map_or_else(
        || "null".to_owned(),
        |s| serde_json::to_string(s).expect("string serializes"),
    );
    let duration_json = duration_seconds.map_or_else(
        || "null".to_owned(),
        |d| serde_json::to_string(&d).expect("int serializes"),
    );
    let raw = format!(
        "{{\"startKnown\":{start_known},\"startedAt\":{started_at_json},\"durationSeconds\":{duration_json}}}"
    );
    serde_json::from_str(&raw).expect("assembled metadata parses")
}

/// Framework-free funnel logic. `S` persists rows; `L` awards XP; `F` captures
/// scorecard facts. The open voice sessions map is public: the gateway
/// adapter clears it on reconnect and tests read it.
pub struct FunnelHandlers<S = MemStore, L = NoopLeveling, F = NoopFacts> {
    store: S,
    leveling: Option<L>,
    facts: Option<F>,
    /// Open voice sessions, so an end can carry a duration.
    pub voice_sessions: Mutex<VoiceSessionTracker>,
}

impl<S: FunnelStore, L: LevelingHook, F: FactsSink> FunnelHandlers<S, L, F> {
    #[must_use]
    pub fn new(store: S, leveling: Option<L>, facts: Option<F>) -> Self {
        Self {
            store,
            leveling,
            facts,
            voice_sessions: Mutex::new(VoiceSessionTracker::new()),
        }
    }

    /// Access the store (replay tests read rows back).
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Member arrival. Bots are captured in facts but never write funnel rows.
    pub fn on_join(&self, i: JoinInput) -> HandlerOutcome {
        if i.is_bot {
            self.store.mark_bot(i.guild_id, i.member_id);
        }
        let at = i.occurred_at.clone().unwrap_or_else(now_iso);
        if let Some(facts) = &self.facts {
            let source_event_id = i
                .source_event_id
                .clone()
                .unwrap_or_else(|| format!("{}:{}:{at}", i.guild_id, i.member_id));
            facts.record_member_join(MemberJoinFact {
                guild_id: i.guild_id,
                member_id: i.member_id,
                is_bot: i.is_bot,
                occurred_at: &at,
                source_event_id: &source_event_id,
                source: &i.source,
                inviter_id: i.inviter_id,
            });
        }
        if i.is_bot {
            return HandlerOutcome::none();
        }
        let event = FunnelEvent {
            guild_id: i.guild_id,
            member_id: Some(i.member_id),
            event_type: EventType::MemberJoin,
            occurred_at: at,
            source: i.source.clone(),
            metadata: i
                .inviter_id
                .map(|id| serde_json::json!({ "inviterId": id.to_string() })),
            dedupe_token: None,
        };
        self.store.record(event.clone());
        HandlerOutcome::event(event)
    }

    /// Rules-gate clearing (TOG-76). Lives here — not in onboarding — because
    /// gate conversion is a membership number recorded on every deployment,
    /// even one that posts no welcome. Once per member by idempotency key, so
    /// a `MemberUpdate` burst cannot inflate it.
    pub fn on_gate_cleared(&self, i: GateClearedInput) -> HandlerOutcome {
        let at = i.occurred_at.clone().unwrap_or_else(now_iso);
        let source = i.source.clone().unwrap_or_else(|| "gateway".to_owned());
        if let Some(facts) = &self.facts {
            facts.record_rules_accepted(RulesAcceptedFact {
                guild_id: i.guild_id,
                member_id: i.member_id,
                is_bot: i.is_bot,
                occurred_at: &at,
                source_event_id: format!("{}:{}:rules", i.guild_id, i.member_id),
                source: &source,
            });
        }
        if i.is_bot {
            return HandlerOutcome::none();
        }
        let event = FunnelEvent {
            guild_id: i.guild_id,
            member_id: Some(i.member_id),
            event_type: EventType::GateCleared,
            occurred_at: at,
            source,
            metadata: None,
            dedupe_token: None,
        };
        self.store.record(event.clone());
        HandlerOutcome::event(event)
    }

    /// Every message updates recency; only the first THREE are funnel
    /// milestones (AM7's text half is "3+ messages", and the third rung is
    /// where that bar clears). After the ladder is full this costs one
    /// indexed read per message and no writes.
    pub fn on_message(&self, i: MessageInput) -> HandlerOutcome {
        let at = i.occurred_at.clone().unwrap_or_else(now_iso);
        if let Some(facts) = &self.facts {
            if let Some(message_id) = &i.message_id {
                facts.record_message(MessageFact {
                    guild_id: i.guild_id,
                    member_id: i.member_id,
                    is_bot: i.is_bot,
                    webhook_id: i.webhook_id,
                    is_staff_automation: i.is_staff_automation,
                    message_id,
                    channel_id: i.channel_id,
                    channel_class: i.channel_class,
                    occurred_at: &at,
                });
            }
        }
        if i.is_bot || i.webhook_id.is_some() || i.is_staff_automation || i.capture_only {
            return HandlerOutcome::none();
        }
        self.store.touch_activity(i.guild_id, i.member_id, &at);
        let mut leveled_up_to = None;
        if let Some(leveling) = &self.leveling {
            let award = leveling.award_message(i.guild_id, i.member_id, &at, i.channel_id);
            if award.leveled_up {
                leveled_up_to = Some(award.level);
            }
        }

        // One message fills at most one rung: the lowest empty one. The loop
        // is for the two-process race — the bot and the website can both read
        // the same empty rung, and the idempotency key lets exactly one have
        // it. It cannot spin: every iteration either fills a rung or finds the
        // ladder full.
        for _ in 0..MESSAGE_RUNGS.len() {
            let Some(rung) = self.store.next_message_rung(i.guild_id, i.member_id, &at) else {
                return HandlerOutcome {
                    event: None,
                    leveled_up_to,
                };
            };
            let event = FunnelEvent {
                guild_id: i.guild_id,
                member_id: Some(i.member_id),
                event_type: rung,
                occurred_at: at.clone(),
                source: format!("channel:{}", i.channel_id),
                metadata: None,
                dedupe_token: None,
            };
            if self.store.record(event.clone()).inserted {
                return HandlerOutcome {
                    event: Some(event),
                    leveled_up_to,
                };
            }
        }
        HandlerOutcome {
            event: None,
            leveled_up_to,
        }
    }

    /// Voice join. Two writes, deliberately separate: `voice_session_start`
    /// every time (the row that makes "how often" answerable), plus
    /// `first_voice_session` once per member. Returns the first-session event
    /// or null, as legacy callers expect.
    pub fn on_voice_join(&self, i: VoiceInput) -> HandlerOutcome {
        let at = i.occurred_at.clone().unwrap_or_else(now_iso);
        let session_key = self.facts.as_ref().and_then(|facts| {
            facts.record_voice_started(VoiceStartedFact {
                guild_id: i.guild_id,
                member_id: i.member_id,
                is_bot: i.is_bot,
                channel_id: i.channel_id,
                occurred_at: &at,
            })
        });
        if i.is_bot {
            return HandlerOutcome::none();
        }
        self.store.touch_activity(i.guild_id, i.member_id, &at);
        self.store.record(FunnelEvent {
            guild_id: i.guild_id,
            member_id: Some(i.member_id),
            event_type: EventType::VoiceSessionStart,
            occurred_at: at.clone(),
            source: format!("channel:{}", i.channel_id),
            metadata: None,
            dedupe_token: None,
        });
        self.voice_sessions.lock().expect("voice lock").start(
            i.guild_id,
            i.member_id,
            i.channel_id,
            at.clone(),
            session_key,
        );

        if self
            .store
            .has_event(i.guild_id, i.member_id, EventType::FirstVoiceSession)
        {
            return HandlerOutcome::none();
        }
        let event = FunnelEvent {
            guild_id: i.guild_id,
            member_id: Some(i.member_id),
            event_type: EventType::FirstVoiceSession,
            occurred_at: at,
            source: format!("channel:{}", i.channel_id),
            metadata: None,
            dedupe_token: None,
        };
        self.store.record(event.clone());
        HandlerOutcome::event(event)
    }

    /// Voice leave. The end is credited to the channel the session OPENED in;
    /// without a seen start we fall back to the leave frame's channel rather
    /// than inventing one. Leveling fires only on measured sessions.
    pub fn on_voice_leave(&self, i: VoiceInput) -> HandlerOutcome {
        let at = i.occurred_at.clone().unwrap_or_else(now_iso);
        let now = now_iso();
        let open = self
            .voice_sessions
            .lock()
            .expect("voice lock")
            .end(i.guild_id, i.member_id);
        let session_key = open.as_ref().and_then(|o| o.session_key.clone());
        let end: VoiceEnd = resolve_voice_end(open, i.channel_id, &at, &now);

        if let Some(facts) = &self.facts {
            let session_key = session_key.unwrap_or_else(|| {
                format!(
                    "{}:{}:unknown-start:{}:{}",
                    i.guild_id, i.member_id, end.end_at, i.channel_id
                )
            });
            facts.record_voice_ended(VoiceEndedFact {
                guild_id: i.guild_id,
                member_id: i.member_id,
                is_bot: i.is_bot,
                session_key,
                channel_id: end.channel_id,
                occurred_at: &end.end_at,
                started_at: end.started_at.as_deref(),
                duration_seconds: end.duration_seconds,
            });
        }
        if i.is_bot {
            return HandlerOutcome::none();
        }
        let event = FunnelEvent {
            guild_id: i.guild_id,
            member_id: Some(i.member_id),
            event_type: EventType::VoiceSessionEnd,
            occurred_at: end.end_at.clone(),
            source: format!("channel:{}", end.channel_id),
            metadata: Some(voice_end_metadata_json(
                end.start_known,
                end.started_at.as_deref(),
                end.duration_seconds,
            )),
            dedupe_token: None,
        };
        self.store.record(event.clone());
        let mut leveled_up_to = None;
        if let (Some(leveling), Some(duration)) = (&self.leveling, end.duration_seconds) {
            if let Ok(duration) = u64::try_from(duration) {
                let award = leveling.award_voice(
                    i.guild_id,
                    i.member_id,
                    duration,
                    &end.end_at,
                    end.channel_id,
                );
                if award.leveled_up {
                    leveled_up_to = Some(award.level);
                }
            }
        }
        // Leaving at T proves presence up to T: recency moves to the stamped
        // time, never raw garbage (which would throw on timestamptz).
        self.store
            .touch_activity(i.guild_id, i.member_id, &end.end_at);
        HandlerOutcome {
            event: Some(event),
            leveled_up_to,
        }
    }

    /// Server leave (TOG-6122). Also a voice leave: Discord drops the member
    /// from voice with no state update, so without closing here the tracker
    /// leaks until the next reconnect clear — and a later session would reuse
    /// the stale start and invent a duration spanning the absence. The
    /// `member_leave` row itself is recorded unconditionally, bots included.
    pub fn on_leave(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        occurred_at: Option<String>,
        is_bot: Option<bool>,
    ) -> HandlerOutcome {
        if is_bot == Some(true) {
            self.store.mark_bot(guild_id, member_id);
        }
        let at = occurred_at.unwrap_or_else(now_iso);
        if self
            .voice_sessions
            .lock()
            .expect("voice lock")
            .peek(guild_id, member_id)
            .is_some()
        {
            let open_channel = self
                .voice_sessions
                .lock()
                .expect("voice lock")
                .peek(guild_id, member_id)
                .map(|o| o.channel_id)
                .unwrap_or(0);
            self.on_voice_leave(VoiceInput {
                guild_id,
                member_id,
                is_bot: is_bot.unwrap_or(false),
                channel_id: open_channel,
                occurred_at: Some(at.clone()),
            });
        }
        let event = FunnelEvent {
            guild_id,
            member_id: Some(member_id),
            event_type: EventType::MemberLeave,
            occurred_at: at,
            source: "gateway".to_owned(),
            metadata: None,
            dedupe_token: None,
        };
        self.store.record(event.clone());
        HandlerOutcome::event(event)
    }

    /// Invite click (B3 redirect posts these in; Discord cannot report them).
    /// The source is `invite:<code>` — the same string joins are attributed
    /// to — so clicks and joins line up with no special case downstream.
    pub fn on_invite_click(
        &self,
        guild_id: Snowflake,
        code: &str,
        occurred_at: Option<String>,
        campaign: Option<String>,
        dedupe_token: Option<String>,
    ) -> HandlerOutcome {
        let event = FunnelEvent {
            guild_id,
            member_id: None,
            event_type: EventType::InviteClick,
            occurred_at: occurred_at.unwrap_or_else(now_iso),
            source: format!("invite:{code}"),
            metadata: campaign.map(|c| serde_json::json!({ "campaign": c })),
            dedupe_token,
        };
        self.store.record(event.clone());
        HandlerOutcome::event(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_iso_millis;

    const G: Snowflake = 1;
    const M: Snowflake = 2;

    fn handlers() -> FunnelHandlers {
        FunnelHandlers::new(MemStore::new(), Some(NoopLeveling), Some(NoopFacts))
    }

    fn join_input(source: &str) -> JoinInput {
        JoinInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            source: source.to_owned(),
            occurred_at: Some("2026-09-20T12:00:00.000Z".to_owned()),
            inviter_id: None,
            source_event_id: None,
        }
    }

    #[test]
    fn join_writes_row_bots_do_not() {
        let h = handlers();
        let out = h.on_join(join_input("invite:abc"));
        assert!(out.event.is_some());
        let bot = h.on_join(JoinInput {
            is_bot: true,
            ..join_input("invite:abc")
        });
        assert!(bot.event.is_none());
    }

    /// Capturing scorecard sink: records every join fact, including bots.
    #[derive(Debug, Default, Clone)]
    struct JoinSpy {
        joins: std::sync::Arc<Mutex<Vec<(String, Option<Snowflake>, String)>>>,
    }

    impl FactsSink for JoinSpy {
        fn record_member_join(&self, fact: MemberJoinFact<'_>) {
            self.joins.lock().expect("spy lock").push((
                fact.source.to_owned(),
                fact.inviter_id,
                fact.source_event_id.to_owned(),
            ));
        }

        fn record_rules_accepted(&self, _fact: RulesAcceptedFact<'_>) {}

        fn record_message(&self, _fact: MessageFact<'_>) {}

        fn record_voice_started(&self, _fact: VoiceStartedFact<'_>) -> Option<String> {
            None
        }

        fn record_voice_ended(&self, _fact: VoiceEndedFact<'_>) {}
    }

    fn spied() -> (FunnelHandlers<MemStore, NoopLeveling, JoinSpy>, JoinSpy) {
        let spy = JoinSpy::default();
        let h = FunnelHandlers::new(MemStore::new(), Some(NoopLeveling), Some(spy.clone()));
        (h, spy)
    }

    #[test]
    fn join_fact_preserves_invite_attribution() {
        // The gateway's invite snapshot + expected-join + vanity attribution
        // reaches the fact unchanged: source, inviter and the gateway
        // `guild:member:joined_at` event id all survive `on_join`.
        let (h, spy) = spied();
        h.on_join(JoinInput {
            source: "invite:abc".to_owned(),
            inviter_id: Some(9),
            source_event_id: Some("1:2:2026-09-20T12:00:00.000Z".to_owned()),
            ..join_input("invite:abc")
        });
        assert_eq!(
            spy.joins.lock().expect("spy lock").as_slice(),
            [(
                "invite:abc".to_owned(),
                Some(9),
                "1:2:2026-09-20T12:00:00.000Z".to_owned()
            )],
        );
    }

    #[test]
    fn bot_join_is_captured_but_never_funnel_counted() {
        // Bots are captured in facts but write no funnel row: the fact fires
        // first and the bot gate returns before `store.record`.
        let (h, spy) = spied();
        let out = h.on_join(JoinInput {
            is_bot: true,
            source: "invite:abc".to_owned(),
            inviter_id: Some(9),
            source_event_id: Some("1:7:2026-09-20T12:00:00.000Z".to_owned()),
            ..join_input("invite:abc")
        });
        assert!(out.event.is_none(), "bots write no funnel row");
        assert_eq!(
            spy.joins.lock().expect("spy lock").as_slice(),
            [(
                "invite:abc".to_owned(),
                Some(9),
                "1:7:2026-09-20T12:00:00.000Z".to_owned()
            )],
            "the bot join is still captured"
        );
    }

    #[test]
    fn gate_clear_defaults_source_gateway() {
        let h = handlers();
        let out = h.on_gate_cleared(GateClearedInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            occurred_at: None,
            source: None,
        });
        assert_eq!(out.event.map(|e| e.source).as_deref(), Some("gateway"));
    }

    #[test]
    fn message_ladder_fills_three_rungs_then_stops() {
        let h = handlers();
        let msg = |at: &str| MessageInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            message_id: Some(format!("msg-{at}")),
            webhook_id: None,
            is_staff_automation: false,
            channel_id: 10,
            channel_class: ChannelClass::Human,
            capture_only: false,
            occurred_at: Some(at.to_owned()),
        };
        let kinds: Vec<Option<EventType>> = [
            "2026-09-20T12:00:00.000Z",
            "2026-09-20T12:01:00.000Z",
            "2026-09-20T12:02:00.000Z",
            "2026-09-20T12:03:00.000Z",
        ]
        .iter()
        .map(|at| h.on_message(msg(at)).event.map(|e| e.event_type))
        .collect();
        assert_eq!(
            kinds,
            [
                Some(EventType::FirstMessage),
                Some(EventType::SecondMessage),
                Some(EventType::ThirdMessage),
                None,
            ]
        );
        // Bots, webhooks, staff automation, capture-only: recency-only, no rung.
        for variant in [
            MessageInput {
                is_bot: true,
                ..msg("2026-09-21T12:00:00.000Z")
            },
            MessageInput {
                webhook_id: Some(99),
                ..msg("2026-09-21T12:00:00.000Z")
            },
            MessageInput {
                is_staff_automation: true,
                ..msg("2026-09-21T12:00:00.000Z")
            },
            MessageInput {
                capture_only: true,
                ..msg("2026-09-21T12:00:00.000Z")
            },
        ] {
            assert!(h.on_message(variant).event.is_none());
        }
    }

    #[test]
    fn redelivered_message_does_not_advance_ladder() {
        let h = handlers();
        let at = "2026-09-20T12:00:00.000Z";
        let first = h.on_message(MessageInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            message_id: Some("m1".to_owned()),
            webhook_id: None,
            is_staff_automation: false,
            channel_id: 10,
            channel_class: ChannelClass::Other,
            capture_only: false,
            occurred_at: Some(at.to_owned()),
        });
        assert_eq!(
            first.event.map(|e| e.event_type),
            Some(EventType::FirstMessage)
        );
        // Same instant redelivery: no second rung.
        let dup = h.on_message(MessageInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            message_id: Some("m1-dup".to_owned()),
            webhook_id: None,
            is_staff_automation: false,
            channel_id: 10,
            channel_class: ChannelClass::Other,
            capture_only: false,
            occurred_at: Some(at.to_owned()),
        });
        assert!(dup.event.is_none());
    }

    #[test]
    fn voice_end_metadata_key_order_matches_legacy() {
        let h = handlers();
        h.on_voice_join(VoiceInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            channel_id: 10,
            occurred_at: Some("2026-09-20T12:00:00.000Z".to_owned()),
        });
        let leave = h
            .on_voice_leave(VoiceInput {
                guild_id: G,
                member_id: M,
                is_bot: false,
                channel_id: 10,
                occurred_at: Some("2026-09-20T12:05:30.000Z".to_owned()),
            })
            .event
            .expect("end row");
        // Byte order matches legacy JSON.stringify({startKnown, startedAt,
        // durationSeconds}) — Postgres TEXT comparison is byte-wise.
        assert_eq!(
            leave.metadata.map(|m| m.to_string()).as_deref(),
            Some(
                "{\"startKnown\":true,\"startedAt\":\"2026-09-20T12:00:00.000Z\",\"durationSeconds\":330}"
            )
        );
    }

    #[test]
    fn voice_join_leave_round_trip_with_duration() {
        let h = handlers();
        let join = h.on_voice_join(VoiceInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            channel_id: 10,
            occurred_at: Some("2026-09-20T12:00:00.000Z".to_owned()),
        });
        assert_eq!(
            join.event.map(|e| e.event_type),
            Some(EventType::FirstVoiceSession)
        );
        let leave = h.on_voice_leave(VoiceInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            channel_id: 10,
            occurred_at: Some("2026-09-20T12:05:30.000Z".to_owned()),
        });
        let event = leave.event.expect("end row");
        assert_eq!(event.source, "channel:10");
        assert_eq!(
            event.metadata,
            Some(serde_json::json!({
                "startKnown": true,
                "startedAt": "2026-09-20T12:00:00.000Z",
                "durationSeconds": 330,
            }))
        );
    }

    #[test]
    fn voice_move_is_end_then_start_at_same_instant() {
        let h = handlers();
        h.on_voice_join(VoiceInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            channel_id: 10,
            occurred_at: Some("2026-09-20T12:00:00.000Z".to_owned()),
        });
        let at = "2026-09-20T12:10:00.000Z";
        let end = h
            .on_voice_leave(VoiceInput {
                guild_id: G,
                member_id: M,
                is_bot: false,
                channel_id: 10,
                occurred_at: Some(at.to_owned()),
            })
            .event
            .expect("end");
        assert_eq!(end.source, "channel:10");
        h.on_voice_join(VoiceInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            channel_id: 11,
            occurred_at: Some(at.to_owned()),
        });
        // Same-tick end(A)+start(B) share the instant but not the key.
        let starts = h
            .store
            .rows()
            .iter()
            .filter(|r| r.event_type == EventType::VoiceSessionStart)
            .count();
        assert_eq!(starts, 2);
    }

    /// One `award_voice` call: guild, member, seconds, stamp, channel.
    type VoiceAward = (Snowflake, Snowflake, u64, String, Snowflake);

    /// Records every voice award so a test can prove the hook was (not) called.
    #[derive(Debug, Default)]
    struct RecordingLeveling {
        voice: Mutex<Vec<VoiceAward>>,
    }

    impl LevelingHook for &RecordingLeveling {
        fn award_message(&self, _: Snowflake, _: Snowflake, _: &str, _: Snowflake) -> LevelOutcome {
            LevelOutcome {
                leveled_up: false,
                level: 0,
            }
        }

        fn award_voice(
            &self,
            guild_id: Snowflake,
            member_id: Snowflake,
            duration_seconds: u64,
            at: &str,
            channel_id: Snowflake,
        ) -> LevelOutcome {
            self.voice.lock().expect("voice awards").push((
                guild_id,
                member_id,
                duration_seconds,
                at.to_owned(),
                channel_id,
            ));
            LevelOutcome {
                leveled_up: false,
                level: 0,
            }
        }
    }

    /// Legacy `3c3e7e8` (#265), handler effect: a malformed leave stamp with
    /// the session start on record writes ONE unknown-start end row stamped
    /// with processing time, closes the session, and awards no voice XP.
    #[test]
    fn malformed_leave_with_open_session_writes_one_unknown_start_end_and_no_xp() {
        let xp = RecordingLeveling::default();
        let h = FunnelHandlers::new(MemStore::new(), Some(&xp), Some(NoopFacts));
        h.on_voice_join(VoiceInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            channel_id: 10,
            occurred_at: Some("2026-09-20T12:00:00.000Z".to_owned()),
        });
        assert_eq!(h.voice_sessions.lock().expect("lock").open_count(), 1);

        let before = parse_iso_millis(&now_iso()).expect("now parses");
        let out = h.on_voice_leave(VoiceInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            channel_id: 99,
            occurred_at: Some("garbage".to_owned()),
        });
        let after = parse_iso_millis(&now_iso()).expect("now parses");

        let ends: Vec<_> = h
            .store
            .rows()
            .into_iter()
            .filter(|r| r.event_type == EventType::VoiceSessionEnd)
            .collect();
        assert_eq!(ends.len(), 1, "exactly one end row");
        let end = &ends[0];
        // Credited to the session's channel, not the leave frame's.
        assert_eq!(end.source, "channel:10");
        assert_ne!(end.occurred_at, "garbage", "garbage is never stored");
        let stamped = parse_iso_millis(&end.occurred_at).expect("stamp parses");
        assert!(
            (before..=after).contains(&stamped),
            "end is stamped with processing time: {} not in {before}..={after}",
            end.occurred_at
        );
        assert_eq!(
            end.metadata,
            Some(serde_json::json!({
                "startKnown": false,
                "startedAt": null,
                "durationSeconds": null,
            }))
        );
        assert_eq!(
            out.event.map(|e| e.occurred_at),
            Some(end.occurred_at.clone())
        );
        assert!(out.leveled_up_to.is_none());
        assert_eq!(
            h.voice_sessions.lock().expect("lock").open_count(),
            0,
            "the unmeasurable session is closed, not left to leak"
        );
        assert!(
            xp.voice.lock().expect("voice awards").is_empty(),
            "no duration, no XP"
        );
        // Recency moves to the stamped time, never the garbage.
        assert_eq!(h.store.activity(G, M), Some(end.occurred_at.clone()));
    }

    /// Control for the test above: the same flow with a readable stamp DOES
    /// award, so the recording double cannot pass vacuously.
    #[test]
    fn well_formed_leave_with_open_session_awards_the_measured_seconds() {
        let xp = RecordingLeveling::default();
        let h = FunnelHandlers::new(MemStore::new(), Some(&xp), Some(NoopFacts));
        h.on_voice_join(VoiceInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            channel_id: 10,
            occurred_at: Some("2026-09-20T12:00:00.000Z".to_owned()),
        });
        h.on_voice_leave(VoiceInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            channel_id: 10,
            occurred_at: Some("2026-09-20T12:05:30.000Z".to_owned()),
        });
        assert_eq!(
            *xp.voice.lock().expect("voice awards"),
            vec![(G, M, 330, "2026-09-20T12:05:30.000Z".to_owned(), 10)]
        );
    }

    #[test]
    fn leave_closes_open_session_then_records() {
        let h = handlers();
        h.on_voice_join(VoiceInput {
            guild_id: G,
            member_id: M,
            is_bot: false,
            channel_id: 10,
            occurred_at: Some("2026-09-20T12:00:00.000Z".to_owned()),
        });
        let out = h.on_leave(
            G,
            M,
            Some("2026-09-20T13:00:00.000Z".to_owned()),
            Some(false),
        );
        assert_eq!(
            out.event.map(|e| e.event_type),
            Some(EventType::MemberLeave)
        );
        assert_eq!(h.voice_sessions.lock().expect("lock").open_count(), 0);
        // Idempotent: second leave writes the row again (repeatable key) but
        // finds no session to close.
        h.on_leave(
            G,
            M,
            Some("2026-09-20T13:00:00.000Z".to_owned()),
            Some(false),
        );
        let ends = h
            .store
            .rows()
            .iter()
            .filter(|r| r.event_type == EventType::VoiceSessionEnd)
            .count();
        assert_eq!(ends, 1);
    }

    #[test]
    fn invite_click_source_matches_join_attribution() {
        let h = handlers();
        let out = h.on_invite_click(
            G,
            "abc",
            None,
            Some("reddit".to_owned()),
            Some("tok".to_owned()),
        );
        let event = out.event.expect("click row");
        assert_eq!(event.source, "invite:abc");
        assert!(event.member_id.is_none());
    }
}
