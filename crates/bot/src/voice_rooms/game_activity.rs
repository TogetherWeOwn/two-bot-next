//! Game-activity analytics feeds (Build slice of `TOG-23402`).
//!
//! Maps gateway [`PresenceUpdate`] activities into
//! [`two_bot_core::game_activity`] presence deltas and derives voice spans
//! from join/leave plus Discord `self_stream` frames. The [`sessionize`] /
//! [`aggregate_guild`] pipeline itself stays in core (pure, offline); this
//! module only feeds it.
//!
//! Containment (presence-probe precedent, `crate` core `presence`): the only
//! output is guild-level aggregates keyed by (guild, game). No per-member
//! row, no per-member reader, no publication. Buffers are in-memory,
//! per-guild bounded, and never leave the process. Durable writes, flush
//! cadence and opt-out gating are a follow-up slice: there is deliberately
//! no migration, store, reader or metric label here.
//!
//! [`sessionize`]: two_bot_core::game_activity::sessionize
//! [`aggregate_guild`]: two_bot_core::game_activity::aggregate_guild
//! [`PresenceUpdate`]: twilight_model::gateway::payload::incoming::PresenceUpdate

use std::collections::HashMap;
use std::sync::Mutex;

use twilight_model::gateway::payload::incoming::PresenceUpdate;
use twilight_model::gateway::presence::{Activity, ActivityButton, ActivityType};
use two_bot_core::game_activity::{
    self, ActivitySnapshot, GuildGameAggregate, PresenceDelta, VoiceSpan,
};

use super::presence::member_id;
use super::Snowflake;

/// Gap timeout for session contiguity, in millis.
///
/// Matches the core tests' practice (15 minutes): observations for the same
/// member and game merge while each step stays within this gap.
pub const GAME_ACTIVITY_GAP_TIMEOUT_MS: i64 = 15 * 60 * 1000;

/// Per-guild cap on buffered presence deltas. Past the cap the oldest deltas
/// are evicted first, so a hot guild cannot grow memory without bound. The
/// count is reported via [`GameActivityCollector::dropped_counts`].
pub const MAX_DELTAS_PER_GUILD: usize = 8_192;

/// Per-guild cap on buffered closed voice spans, same eviction contract as
/// deltas.
pub const MAX_SPANS_PER_GUILD: usize = 8_192;

/// Map one gateway presence into an analytics delta.
///
/// Keeps Playing (type 0) and Streaming (type 1) activities with their full
/// fields; Listening / Watching / Competing / Custom / Unknown never create
/// a session, so they map to no snapshot (an empty delta still closes the
/// member's open interval in [`sessionize`], which is the pinned contract).
/// Every kept snapshot passes through [`sanitize_activity`](game_activity::sanitize_activity):
/// button labels survive, every button URL is dropped, and a stream URL
/// survives only for Twitch / YouTube. Activity-local timestamps are
/// intentionally ignored: sessionization is poller-observation semantics, so
/// `observed_at_ms` (the gateway receipt clock) is the observation time.
#[must_use]
pub fn presence_delta(update: &PresenceUpdate, observed_at_ms: i64) -> PresenceDelta {
    PresenceDelta {
        user_key: member_id(&update.0).to_string(),
        observed_at_ms,
        activities: update
            .0
            .activities
            .iter()
            .filter_map(activity_snapshot)
            .collect(),
    }
}

fn activity_snapshot(activity: &Activity) -> Option<ActivitySnapshot> {
    let activity_type = match activity.kind {
        ActivityType::Playing => game_activity::ActivityType::Playing,
        ActivityType::Streaming => game_activity::ActivityType::Streaming,
        _ => return None,
    };
    let mut button_labels = Vec::new();
    let mut button_urls = Vec::new();
    for button in &activity.buttons {
        match button {
            ActivityButton::Link(link) => {
                button_labels.push(link.label.clone());
                button_urls.push(link.url.clone());
            }
            ActivityButton::Text(text) => button_labels.push(text.label.clone()),
            ActivityButton::Unknown => {}
        }
    }
    let snapshot =
        ActivitySnapshot {
            activity_type,
            name: activity.name.clone(),
            application_id: activity.application_id.map(|id| id.to_string()),
            details: activity.details.clone(),
            state: activity.state.clone(),
            party_size: activity.party.as_ref().and_then(|party| party.size).map(
                |[current, max]| {
                    const U32_MAX: u64 = u32::MAX as u64;
                    [current.min(U32_MAX) as u32, max.min(U32_MAX) as u32]
                },
            ),
            assets: activity.assets.is_some(),
            button_labels,
            button_urls,
            stream_url: activity.url.clone(),
        };
    Some(game_activity::sanitize_activity(&snapshot))
}

/// Open voice span for one member: joined, not yet left.
struct OpenSpan {
    join_ms: i64,
    self_stream: bool,
}

#[derive(Default)]
struct CollectorInner {
    deltas: HashMap<Snowflake, Vec<PresenceDelta>>,
    spans: HashMap<Snowflake, Vec<VoiceSpan>>,
    open: HashMap<(Snowflake, Snowflake), OpenSpan>,
    dropped_deltas: u64,
    dropped_spans: u64,
}

/// In-memory game-activity feeds, keyed per guild.
///
/// Fed from the voice runtime's presence and voice frames; [`flush`] runs
/// the core pipeline and drains the consumed buffers. Open voice spans are
/// retained across flushes until the member leaves. `user_key`s never leave
/// this struct except inside the ephemeral [`sessionize`] call; the returned
/// aggregates carry no member key.
///
/// [`flush`]: GameActivityCollector::flush
/// [`sessionize`]: game_activity::sessionize
#[derive(Default)]
pub struct GameActivityCollector {
    inner: Mutex<CollectorInner>,
}

impl GameActivityCollector {
    /// Empty collector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Buffer one mapped presence delta for `guild`, evicting oldest first
    /// past [`MAX_DELTAS_PER_GUILD`].
    pub fn observe_presence(&self, guild: Snowflake, delta: PresenceDelta) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let buffer = inner.deltas.entry(guild).or_default();
        buffer.push(delta);
        if buffer.len() > MAX_DELTAS_PER_GUILD {
            let overflow = buffer.len() - MAX_DELTAS_PER_GUILD;
            buffer.drain(..overflow);
            inner.dropped_deltas += overflow as u64;
        }
    }

    /// Open a voice span for `member` at `now_ms`, keeping the original join
    /// when one is already open (idempotent: duplicate joins and channel
    /// moves never restart the span).
    pub fn observe_join(&self, guild: Snowflake, member: Snowflake, now_ms: i64) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.open.entry((guild, member)).or_insert(OpenSpan {
            join_ms: now_ms,
            self_stream: false,
        });
    }

    /// Close `member`'s open span at `now_ms` into the span buffer,
    /// evicting oldest first past [`MAX_SPANS_PER_GUILD`]. A leave with no
    /// open span is ignored.
    pub fn observe_leave(&self, guild: Snowflake, member: Snowflake, now_ms: i64) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let Some(open) = inner.open.remove(&(guild, member)) else {
            return;
        };
        let buffer = inner.spans.entry(guild).or_default();
        buffer.push(VoiceSpan {
            user_key: member.to_string(),
            join_ms: open.join_ms,
            leave_ms: now_ms,
            self_stream: open.self_stream,
        });
        if buffer.len() > MAX_SPANS_PER_GUILD {
            let overflow = buffer.len() - MAX_SPANS_PER_GUILD;
            buffer.drain(..overflow);
            inner.dropped_spans += overflow as u64;
        }
    }

    /// Fold Discord `self_stream` into `member`'s open span. A `true` flag
    /// with no open span opens one at `now_ms` (the member is evidently in
    /// voice; the join time is the honest observable point). The flag never
    /// creates overlap on its own: overlap needs shared time with a span.
    pub fn observe_self_stream(
        &self,
        guild: Snowflake,
        member: Snowflake,
        streaming: bool,
        now_ms: i64,
    ) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        match inner.open.get_mut(&(guild, member)) {
            Some(open) => open.self_stream |= streaming,
            None if streaming => {
                inner.open.insert(
                    (guild, member),
                    OpenSpan {
                        join_ms: now_ms,
                        self_stream: true,
                    },
                );
            }
            None => {}
        }
    }

    /// Run the core pipeline over `guild`'s buffered deltas and closed spans
    /// and drain them. Returns guild-level aggregates only. Open spans stay
    /// until their member leaves; an interval spanning a flush boundary
    /// splits there, so production cadence should flush well past
    /// [`GAME_ACTIVITY_GAP_TIMEOUT_MS`].
    #[must_use]
    pub fn flush(&self, guild: Snowflake, gap_timeout_ms: i64) -> Vec<GuildGameAggregate> {
        let (deltas, spans) = {
            let Ok(mut inner) = self.inner.lock() else {
                return Vec::new();
            };
            let deltas = inner.deltas.remove(&guild).unwrap_or_default();
            let spans = inner.spans.remove(&guild).unwrap_or_default();
            (deltas, spans)
        };
        let sessions = game_activity::sessionize(&deltas, &spans, gap_timeout_ms);
        game_activity::aggregate_guild(&guild.to_string(), &sessions)
    }

    /// Evicted-buffer counters `(deltas, spans)` for tests and operators.
    #[must_use]
    pub fn dropped_counts(&self) -> (u64, u64) {
        let Ok(inner) = self.inner.lock() else {
            return (0, 0);
        };
        (inner.dropped_deltas, inner.dropped_spans)
    }

    /// Buffered `(deltas, closed spans, open spans)` for `guild`, for tests.
    #[must_use]
    #[cfg(test)]
    fn buffered_counts(&self, guild: Snowflake) -> (usize, usize, usize) {
        let Ok(inner) = self.inner.lock() else {
            return (0, 0, 0);
        };
        let deltas = inner.deltas.get(&guild).map_or(0, Vec::len);
        let spans = inner.spans.get(&guild).map_or(0, Vec::len);
        let open = inner
            .open
            .keys()
            .filter(|(span_guild, _)| *span_guild == guild)
            .count();
        (deltas, spans, open)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use two_bot_core::game_activity::ActivityType as CoreActivityType;

    const T0: i64 = 1_750_000_000_000;
    const MIN: i64 = 60_000;

    fn update(activities: serde_json::Value) -> PresenceUpdate {
        serde_json::from_value(json!({
            "user": {"id": "42"},
            "guild_id": "7",
            "status": "online",
            "activities": activities,
            "client_status": {"desktop": "online"}
        }))
        .expect("presence update fixture")
    }

    fn playing_activity(name: &str) -> serde_json::Value {
        json!({
            "type": 0,
            "name": name,
            "application_id": "123",
            "details": "Ranked",
            "state": "In queue",
            "party": {"id": "p", "size": [2, 3]},
            "assets": {"large_image": "apex"},
            "buttons": [
                {"label": "Join", "url": "https://evil.example/join"},
                "Watch"
            ],
            "timestamps": {"start": 1_750_000_000_000u64},
        })
    }

    #[test]
    fn playing_maps_full_fields_and_sanitizes_urls() {
        let delta = presence_delta(&update(json!([playing_activity("Apex Legends")])), T0);
        assert_eq!(delta.user_key, "42", "member key is the presence user");
        assert_eq!(
            delta.observed_at_ms, T0,
            "observation time is the receipt clock"
        );
        assert_eq!(delta.activities.len(), 1);
        let snapshot = &delta.activities[0];
        assert_eq!(snapshot.activity_type, CoreActivityType::Playing);
        assert_eq!(snapshot.name, "Apex Legends");
        assert_eq!(snapshot.application_id.as_deref(), Some("123"));
        assert_eq!(snapshot.details.as_deref(), Some("Ranked"));
        assert_eq!(snapshot.state.as_deref(), Some("In queue"));
        assert_eq!(snapshot.party_size, Some([2, 3]));
        assert!(snapshot.assets, "assets survive mapping");
        assert_eq!(
            snapshot.button_labels,
            vec!["Join".to_owned(), "Watch".to_owned()],
            "button labels survive in order"
        );
        assert!(
            snapshot.button_urls.is_empty(),
            "every button URL is dropped, got {:?}",
            snapshot.button_urls
        );
        assert!(
            snapshot.stream_url.is_none(),
            "play activities carry no stream URL"
        );
    }

    #[test]
    fn streaming_maps_and_stream_url_allowlist_enforced() {
        let twitch = update(json!([{
            "type": 1, "name": "Grinding",
            "url": "https://twitch.tv/someone",
        }]));
        let delta = presence_delta(&twitch, T0);
        assert_eq!(delta.activities.len(), 1);
        assert_eq!(
            delta.activities[0].activity_type,
            CoreActivityType::Streaming
        );
        assert_eq!(
            delta.activities[0].stream_url.as_deref(),
            Some("https://twitch.tv/someone"),
            "allowlisted stream URL survives"
        );
        let evil = update(json!([{
            "type": 1, "name": "Grinding",
            "url": "https://twitch.evil.example/x",
        }]));
        let delta = presence_delta(&evil, T0);
        assert_eq!(delta.activities.len(), 1);
        assert!(
            delta.activities[0].stream_url.is_none(),
            "lookalike stream URL is dropped"
        );
    }

    #[test]
    fn non_session_types_yield_empty_delta() {
        for numeric in [2u8, 3, 4, 5, 99] {
            let delta = presence_delta(&update(json!([{"type": numeric, "name": "Whatever"}])), T0);
            assert!(
                delta.activities.is_empty(),
                "gateway type {numeric} must map to no snapshot"
            );
            assert_eq!(delta.user_key, "42");
            assert_eq!(delta.observed_at_ms, T0);
        }
    }

    #[test]
    fn empty_activities_yield_empty_delta() {
        let delta = presence_delta(&update(json!([])), T0);
        assert!(delta.activities.is_empty());
        assert_eq!(delta.user_key, "42");
    }

    #[test]
    fn collector_flush_folds_sessions_and_voice_overlap() {
        let collector = GameActivityCollector::new();
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0 + 30 * MIN),
        );
        collector.observe_join(7, 42, T0);
        collector.observe_leave(7, 42, T0 + MIN);
        let rows = collector.flush(7, 60 * MIN);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].guild_id, "7");
        assert_eq!(rows[0].game.as_deref(), Some("Apex"));
        assert_eq!(rows[0].total_play_ms, 30 * MIN, "wall-clock span");
        assert_eq!(
            rows[0].in_voice_ms, MIN,
            "1-minute overlap, not the whole 30-minute span"
        );
        assert_eq!(rows[0].session_count, 1);
        assert_eq!(
            collector.buffered_counts(7),
            (0, 0, 0),
            "flush drains consumed buffers"
        );
    }

    #[test]
    fn other_members_voice_and_non_playing_close_only_self() {
        let collector = GameActivityCollector::new();
        // Member 42 plays at T0 and T0+10m; member 43 plays at T0+2m then
        // goes idle at T0+3m; member 44's voice span overlaps everything.
        let mut delta_a = presence_delta(&update(json!([playing_activity("Apex")])), T0);
        delta_a.user_key = "42".to_owned();
        let mut delta_a2 =
            presence_delta(&update(json!([playing_activity("Apex")])), T0 + 10 * MIN);
        delta_a2.user_key = "42".to_owned();
        let mut delta_b = presence_delta(&update(json!([playing_activity("Apex")])), T0 + 2 * MIN);
        delta_b.user_key = "43".to_owned();
        let mut idle_b = presence_delta(&update(json!([])), T0 + 3 * MIN);
        idle_b.user_key = "43".to_owned();
        for delta in [delta_a, delta_b, idle_b, delta_a2] {
            collector.observe_presence(7, delta);
        }
        collector.observe_join(7, 44, T0 - MIN);
        collector.observe_leave(7, 44, T0 + 20 * MIN);
        let rows = collector.flush(7, 15 * MIN);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_count, 2, "one session per member");
        assert_eq!(
            rows[0].total_play_ms,
            11 * MIN,
            "42 spans 10 min plus 43's 1-minute floor"
        );
        assert_eq!(
            rows[0].in_voice_ms, 0,
            "44's span must not leak into 42/43 sessions"
        );
    }

    #[test]
    fn aggregates_carry_no_member_key() {
        let collector = GameActivityCollector::new();
        let mut delta = presence_delta(&update(json!([playing_activity("Apex")])), T0);
        delta.user_key = "42".to_owned();
        collector.observe_presence(7, delta);
        let rows = collector.flush(7, 15 * MIN);
        assert_eq!(rows.len(), 1);
        let debug = format!("{:?}", rows[0]);
        assert!(
            !debug.contains("42"),
            "aggregate Debug must carry no member key, got: {debug}"
        );
        let json = serde_json::to_string(&rows[0]).expect("aggregate serializes");
        assert!(
            !json.contains("42"),
            "aggregate JSON must carry no member key, got: {json}"
        );
        assert!(
            !json.contains("user_key") && !json.contains("member"),
            "aggregate JSON must have no member field, got: {json}"
        );
    }

    #[test]
    fn join_is_idempotent_move_keeps_span_leave_closes() {
        let collector = GameActivityCollector::new();
        collector.observe_join(7, 42, T0);
        collector.observe_join(7, 42, T0 + 5 * MIN);
        collector.observe_join(7, 42, T0 + 9 * MIN);
        assert_eq!(collector.buffered_counts(7), (0, 0, 1), "one open span");
        collector.observe_leave(7, 42, T0 + 10 * MIN);
        assert_eq!(collector.buffered_counts(7), (0, 1, 0), "leave closes");
        collector.observe_leave(7, 42, T0 + 11 * MIN);
        assert_eq!(
            collector.buffered_counts(7),
            (0, 1, 0),
            "leave with no open span is ignored"
        );
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0 + 30 * MIN),
        );
        let rows = collector.flush(7, 60 * MIN);
        assert_eq!(
            rows[0].in_voice_ms,
            10 * MIN,
            "span runs from the first join, not the duplicates"
        );
    }

    #[test]
    fn self_stream_needs_shared_time_like_any_span() {
        let collector = GameActivityCollector::new();
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        // Disjoint self-stream span contributes nothing.
        collector.observe_join(7, 42, T0 + 60 * MIN);
        collector.observe_self_stream(7, 42, true, T0 + 60 * MIN);
        collector.observe_leave(7, 42, T0 + 70 * MIN);
        let rows = collector.flush(7, 15 * MIN);
        assert_eq!(rows[0].in_voice_ms, 0);
        // Overlapping self-stream span counts like any span.
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        collector.observe_join(7, 42, T0 - MIN);
        collector.observe_self_stream(7, 42, true, T0);
        collector.observe_leave(7, 42, T0 + MIN);
        let rows = collector.flush(7, 15 * MIN);
        assert_eq!(rows[0].in_voice_ms, MIN);
    }

    #[test]
    fn buffers_are_bounded_and_flush_still_works() {
        let collector = GameActivityCollector::new();
        for step in 0..(MAX_DELTAS_PER_GUILD + 100) {
            let mut delta = presence_delta(
                &update(json!([playing_activity("Apex")])),
                T0 + i64::try_from(step).unwrap_or(i64::MAX) * MIN,
            );
            delta.user_key = "42".to_owned();
            collector.observe_presence(7, delta);
        }
        let (deltas, _, _) = collector.buffered_counts(7);
        assert_eq!(deltas, MAX_DELTAS_PER_GUILD, "oldest deltas evicted first");
        assert_eq!(collector.dropped_counts().0, 100);
        let rows = collector.flush(7, 15 * MIN);
        assert!(!rows.is_empty(), "bounded buffers still sessionize");
    }
}
