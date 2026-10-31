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
//! row, no per-member reader, no publication. Buffers are in-memory and
//! per-guild bounded; the hourly `flush_loop` persists each due guild
//! through the opt-out gate (`persist_guild`) into the core store, with
//! label-free count-only telemetry and no metric label.
//!
//! [`sessionize`]: two_bot_core::game_activity::sessionize
//! [`aggregate_guild`]: two_bot_core::game_activity::aggregate_guild
//! [`PresenceUpdate`]: twilight_model::gateway::payload::incoming::PresenceUpdate

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use twilight_model::gateway::payload::incoming::PresenceUpdate;
use twilight_model::gateway::presence::{Activity, ActivityButton, ActivityType};
use two_bot_core::game_activity::{
    self, ActivitySnapshot, ActivityType as CoreActivityType, GuildGameAggregate, PresenceDelta,
    VoiceSpan,
};
use two_bot_core::game_activity_store::{self, GameActivityStoreError};

use super::presence::member_id;
use super::Snowflake;

/// Gap timeout for session contiguity, in millis.
///
/// Matches the core tests' practice (15 minutes): observations for the same
/// member and game merge while each step stays within this gap.
pub const GAME_ACTIVITY_GAP_TIMEOUT_MS: i64 = 15 * 60 * 1000;

/// Persist cadence per guild, in millis: hourly, four times the session gap.
///
/// Well past [`GAME_ACTIVITY_GAP_TIMEOUT_MS`] on purpose, so steady play
/// sessionizes whole while an interval spanning a flush boundary honestly
/// splits there (see [`GameActivityCollector::flush`]).
pub const GAME_ACTIVITY_FLUSH_INTERVAL_MS: i64 = 60 * 60 * 1000;

/// How often the flush loop wakes to check per-guild due times. Wakeups are
/// cheap integer compares; persists still happen at most hourly per guild.
const FLUSH_POLL_INTERVAL: Duration = Duration::from_secs(300);

/// True when `guild` is due for a persist: never flushed, or its last flush
/// is at least [`GAME_ACTIVITY_FLUSH_INTERVAL_MS`] ago. Pure, so the loop
/// below and its tests share one definition of due.
#[must_use]
pub fn flush_due(last_flush_ms: Option<i64>, now_ms: i64) -> bool {
    match last_flush_ms {
        None => true,
        Some(last) => now_ms.saturating_sub(last) >= GAME_ACTIVITY_FLUSH_INTERVAL_MS,
    }
}

/// Per-guild cap on buffered presence deltas. Past the cap the oldest deltas
/// are evicted first, so a hot guild cannot grow memory without bound. The
/// count is reported via [`GameActivityCollector::dropped_counts`].
pub const MAX_DELTAS_PER_GUILD: usize = 8_192;

/// Per-guild cap on buffered closed voice spans, same eviction contract as
/// deltas.
pub const MAX_SPANS_PER_GUILD: usize = 8_192;

/// How long a member's carried play state survives with no fresh observation.
///
/// The gateway only sends presence on change, so a silent member is assumed
/// still playing and each flush carries their open interval forward. Past
/// this bound the carry is dropped instead: a much later same-game delta
/// starts a new interval rather than retroactively crediting the whole gap.
/// A full day of uninterrupted play with zero presence updates is
/// implausible (any status or activity change re-asserts), while a missed
/// stop event plus a replay inside the bound still sessionizes whole.
pub const GAME_ACTIVITY_CARRY_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Cap on carried play states across all guilds. Past the cap an observation
/// first sweeps TTL-stale entries; the map otherwise only holds members with
/// no stop observed yet, so steady state stays far below this.
const MAX_CARRY_ENTRIES: usize = 16_384;

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
///
/// [`sessionize`]: game_activity::sessionize
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

/// One member's play state carried across flushes.
///
/// Gateway presence is change-driven, not polled: the absence of updates
/// means "unchanged", so a member whose last delta asserted a game is
/// assumed still playing it until a stop (or different game) arrives. The
/// next flush reopens their interval from `from_ms` instead of starting
/// over, and drops the carry past [`GAME_ACTIVITY_CARRY_TTL_MS`] with no
/// fresh observation. A stop does not drop the carry (the run-up to it still
/// bridges at flush time) but stamps `stopped_at`, so a later different game
/// in the same window closes the old run at the stop instead of bridging
/// straight across the idle gap. A re-assert after a stop stamps
/// `resumed_at`, so a later game change bridges only the post-resume leg
/// instead of crediting the idle gap as play.
#[derive(Debug, Clone)]
struct PlayCarry {
    game: String,
    is_streaming: bool,
    /// The instant the next window resumes from (last flush, or the last
    /// real observation when the flush clock ran behind it).
    from_ms: i64,
    /// The last real observation time, guarding against out-of-order frames.
    last_ms: i64,
    /// The stop time when the member's last observation was an empty delta.
    /// `None` while a game is asserted. A re-asserted game clears it; a game
    /// change ends its close-out here instead of at the change clock.
    stopped_at: Option<i64>,
    /// When the current game resumed after a stop (re-assert time).
    /// `None` for a freshly asserted game. A game change starts its bridge
    /// here instead of at `from_ms`, so the idle gap stays uncredited while
    /// the pre-stop run-up still bridges via the reopen point.
    resumed_at: Option<i64>,
}

#[derive(Default)]
struct CollectorInner {
    deltas: HashMap<Snowflake, Vec<PresenceDelta>>,
    spans: HashMap<Snowflake, Vec<VoiceSpan>>,
    open: HashMap<(Snowflake, Snowflake), OpenSpan>,
    carry: HashMap<(Snowflake, String), PlayCarry>,
    dropped_deltas: u64,
    dropped_spans: u64,
}

/// In-memory game-activity feeds, keyed per guild.
///
/// Fed from the voice runtime's presence and voice frames; [`flush`] runs
/// the core pipeline and drains the consumed buffers. Open voice spans are
/// clipped at each flush (the pre-flush portion sessionizes now, the open
/// restarts at the flush clock) and retained until the member leaves. Each
/// member's current game is carried across flushes the same way: a still
/// playing tail closes at the flush clock and reopens for the next window,
/// so change-driven presence with hours between updates sessionizes whole.
/// `user_key`s never leave this struct except inside the ephemeral
/// [`sessionize`] call; the returned aggregates carry no member key.
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
    /// past [`MAX_DELTAS_PER_GUILD`]. Also tracks the member's current game
    /// as a state transition: a game delta opens (or refreshes) their carry,
    /// an empty delta stamps it for the flush to close (the run-up to the
    /// stop still bridges there). Out-of-order frames older than the tracked
    /// state still buffer (sessionize sorts) but never clobber it.
    pub fn observe_presence(&self, guild: Snowflake, delta: PresenceDelta) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let observed_at_ms = delta.observed_at_ms;
        let user_key = delta.user_key.clone();
        let asserted = game_activity::session_game(&delta);
        let overflow = {
            let buffer = inner.deltas.entry(guild).or_default();
            buffer.push(delta);
            let overflow = buffer.len().saturating_sub(MAX_DELTAS_PER_GUILD);
            if overflow > 0 {
                buffer.drain(..overflow);
            }
            overflow
        };
        if overflow > 0 {
            inner.dropped_deltas += overflow as u64;
        }
        // Synthetic old-game close-out collected here and flushed into the
        // delta buffer after the carry borrow ends, so no buffer borrow lives
        // across a carry or counter mutation.
        let mut pending: Vec<PresenceDelta> = Vec::new();
        match inner.carry.entry((guild, user_key.clone())) {
            Entry::Occupied(mut stated) if observed_at_ms >= stated.get().last_ms => {
                match asserted {
                    Some((game, is_streaming)) => {
                        let prev = stated.get().clone();
                        // The same game continuing (even across a stop) keeps
                        // the earlier reopen point, so the pre-stop span still
                        // bridges at flush time. A changed game restarts it.
                        if prev.game == game && prev.is_streaming == is_streaming {
                            let from_ms = prev.from_ms.min(observed_at_ms);
                            // A re-assert after a stop resumes play there: the
                            // earlier stop no longer bounds the run, but the
                            // idle gap must stay uncredited when a later game
                            // change closes this run.
                            let resumed_at = if prev.stopped_at.is_some() {
                                Some(observed_at_ms)
                            } else {
                                prev.resumed_at
                            };
                            stated.insert(PlayCarry {
                                game,
                                is_streaming,
                                from_ms,
                                last_ms: observed_at_ms,
                                // A re-asserted game resumes play: the earlier
                                // stop no longer bounds the run.
                                stopped_at: None,
                                resumed_at,
                            });
                        } else {
                            // A game change closes the previous game: the old
                            // run-up is materialized into the delta buffer now
                            // (reopen point, bridge ticks, closing point); the
                            // flush would otherwise never replay it after the
                            // carry below switches to the new game. TTL-stale
                            // carries are dropped instead of credited. When the
                            // old game already stopped (`stopped_at`), the run
                            // ended at the stop: bridging to the change clock
                            // would credit the idle gap as phantom play, so
                            // only the reopen point is emitted and the flush's
                            // own stop-bridge replays the run-up to the stop.
                            // The stop delta is already in this window's buffer
                            // when the stop and the change share a window; when
                            // the old game was carried over from an earlier
                            // flush, the reopen point still precedes the stop
                            // so the run-up from the last flush is not lost.
                            let stopped_bounds_run = matches!(
                                prev.stopped_at,
                                Some(stopped_at)
                                    if stopped_at >= prev.from_ms
                                        && stopped_at <= observed_at_ms
                            );
                            if observed_at_ms >= prev.from_ms
                                && observed_at_ms.saturating_sub(prev.from_ms)
                                    <= GAME_ACTIVITY_CARRY_TTL_MS
                            {
                                pending.push(synthetic_delta(
                                    &user_key,
                                    &prev.game,
                                    prev.is_streaming,
                                    prev.from_ms,
                                ));
                                if !stopped_bounds_run {
                                    // A resume inside the bridged range restarts
                                    // the bridge there: the pre-stop leg still
                                    // bridges via the reopen point above, while
                                    // the idle gap stays uncredited.
                                    let bridge_from_ms = prev.resumed_at.unwrap_or(prev.from_ms);
                                    fill_ticks(
                                        &mut pending,
                                        &user_key,
                                        &prev.game,
                                        prev.is_streaming,
                                        bridge_from_ms,
                                        observed_at_ms,
                                        GAME_ACTIVITY_GAP_TIMEOUT_MS,
                                    );
                                    pending.push(synthetic_delta(
                                        &user_key,
                                        &prev.game,
                                        prev.is_streaming,
                                        observed_at_ms,
                                    ));
                                }
                            }
                            stated.insert(PlayCarry {
                                game,
                                is_streaming,
                                from_ms: observed_at_ms,
                                last_ms: observed_at_ms,
                                stopped_at: None,
                                resumed_at: None,
                            });
                        }
                    }
                    None => {
                        // A stop closes the interval at flush time; the carry
                        // stays so the run-up to the stop still bridges there
                        // (the flush drops it once closed). The stop clock is
                        // stamped so a later different game in the same window
                        // ends the old run here, not at the change clock.
                        let prev = stated.get().clone();
                        stated.insert(PlayCarry {
                            last_ms: observed_at_ms,
                            stopped_at: Some(observed_at_ms),
                            ..prev
                        });
                    }
                }
            }
            Entry::Vacant(slot) => {
                if let Some((game, is_streaming)) = asserted {
                    slot.insert(PlayCarry {
                        game,
                        is_streaming,
                        from_ms: observed_at_ms,
                        last_ms: observed_at_ms,
                        stopped_at: None,
                        resumed_at: None,
                    });
                }
            }
            Entry::Occupied(_) => {}
        }
        if !pending.is_empty() {
            let pending_len = pending.len();
            let overflow = {
                let buffer = inner.deltas.entry(guild).or_default();
                buffer.extend(pending);
                // Game-change close-out must sort before the change delta:
                // stable sort keeps buffer order for equal timestamps, so
                // [delta, syns] would interleave the new game between the
                // old game's closing points and split it into floors.
                // Rotate to [syns, delta] so sessionize closes old first.
                let len = buffer.len();
                if len > pending_len {
                    buffer[len - pending_len - 1..].rotate_left(1);
                }
                let overflow = buffer.len().saturating_sub(MAX_DELTAS_PER_GUILD);
                if overflow > 0 {
                    buffer.drain(..overflow);
                }
                overflow
            };
            if overflow > 0 {
                inner.dropped_deltas += overflow as u64;
            }
        }
        if inner.carry.len() > MAX_CARRY_ENTRIES {
            inner.carry.retain(|_, stated| {
                observed_at_ms.saturating_sub(stated.from_ms) <= GAME_ACTIVITY_CARRY_TTL_MS
            });
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

    /// Reconcile open spans against an authoritative snapshot (`GUILD_CREATE`):
    /// members absent from `present` close at `now_ms`, members present open
    /// idempotently (a later join for the same member keeps the earlier join).
    /// A leave lost during a non-resumed reconnect must not inflate voice
    /// time: without this the stale span clips and persists every flush, and
    /// the eventual leave closes a span covering the whole offline gap.
    /// Closes at or before the join (clock skew) are left open.
    pub fn reconcile_voice_snapshot(&self, guild: Snowflake, present: &[Snowflake], now_ms: i64) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let mut closed: Vec<VoiceSpan> = Vec::new();
        inner.open.retain(|(span_guild, member), open| {
            if *span_guild != guild || present.contains(member) || now_ms <= open.join_ms {
                return true;
            }
            closed.push(VoiceSpan {
                user_key: member.to_string(),
                join_ms: open.join_ms,
                leave_ms: now_ms,
                self_stream: open.self_stream,
            });
            false
        });
        if !closed.is_empty() {
            let buffer = inner.spans.entry(guild).or_default();
            buffer.extend(closed);
            if buffer.len() > MAX_SPANS_PER_GUILD {
                let overflow = buffer.len() - MAX_SPANS_PER_GUILD;
                buffer.drain(..overflow);
                inner.dropped_spans += overflow as u64;
            }
        }
        for member in present {
            inner.open.entry((guild, *member)).or_insert(OpenSpan {
                join_ms: now_ms,
                self_stream: false,
            });
        }
    }

    /// Close every open span at `now_ms`: transport loss invalidates live
    /// state, so opens must not cover the offline gap.
    pub fn close_all_opens(&self, now_ms: i64) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let mut closed: Vec<(Snowflake, VoiceSpan)> = Vec::new();
        inner.open.retain(|(guild, member), open| {
            if now_ms <= open.join_ms {
                return true;
            }
            closed.push((
                *guild,
                VoiceSpan {
                    user_key: member.to_string(),
                    join_ms: open.join_ms,
                    leave_ms: now_ms,
                    self_stream: open.self_stream,
                },
            ));
            false
        });
        for (guild, span) in closed {
            let buffer = inner.spans.entry(guild).or_default();
            buffer.push(span);
            if buffer.len() > MAX_SPANS_PER_GUILD {
                let overflow = buffer.len() - MAX_SPANS_PER_GUILD;
                buffer.drain(..overflow);
                inner.dropped_spans += overflow as u64;
            }
        }
    }

    /// Drop every buffered and open state for `guild` (removal: nothing for
    /// the guild persists after this, and a rejoin starts fresh).
    pub fn clear_guild(&self, guild: Snowflake) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.deltas.remove(&guild);
        inner.spans.remove(&guild);
        inner.open.retain(|(span_guild, _), _| *span_guild != guild);
        inner
            .carry
            .retain(|(carry_guild, _), _| *carry_guild != guild);
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

    /// Run the core pipeline over `guild`'s buffered deltas and voice spans
    /// at `now_ms` and drain them. Returns guild-level aggregates only.
    ///
    /// Each open span is clipped at `now_ms`: the pre-flush portion
    /// sessionizes in this window and the open restarts at `now_ms`, so a
    /// member still in voice counts their time instead of persisting zero.
    /// Each still-playing member's interval closes at `now_ms` and reopens
    /// from there via the carry map, so an interval spanning a flush
    /// boundary splits there honestly (production cadence should flush well
    /// past [`GAME_ACTIVITY_GAP_TIMEOUT_MS`]). Members with no fresh delta
    /// in this window contribute nothing; their carry waits for the next
    /// window, subject to [`GAME_ACTIVITY_CARRY_TTL_MS`]. When nothing
    /// sessionizes but a carry is still open, the drained spans are requeued
    /// rather than dropped: overlap is an intersection, so they can only
    /// count against a future bridged session, never twice. When other
    /// members do persist rows, a silent carried member's spans are still
    /// retained the same way (spans of members with sessions stay consumed).
    #[must_use]
    pub fn flush(
        &self,
        guild: Snowflake,
        gap_timeout_ms: i64,
        now_ms: i64,
    ) -> Vec<GuildGameAggregate> {
        let (deltas, spans) = self.drain_guild_at(guild, now_ms);
        let carry = self.guild_carry(guild);
        let (input, set, drop) = plan_flush(&deltas, &carry, gap_timeout_ms, now_ms);
        // Busy-guild case: a carried member with no fresh delta in this
        // window will contribute no session, but their clipped span portion
        // was still drained. Remember them now (`drop` moves into the apply
        // below); their spans are retained after sessionization so the future
        // bridged session overlaps them. Overlap is per-member, so retained
        // spans never counted against another member's session.
        let fresh: std::collections::BTreeSet<&str> =
            deltas.iter().map(|delta| delta.user_key.as_str()).collect();
        let dropped: std::collections::BTreeSet<&str> = drop.iter().map(String::as_str).collect();
        let silent: std::collections::BTreeSet<String> = carry
            .keys()
            .filter(|user| !fresh.contains(user.as_str()) && !dropped.contains(user.as_str()))
            .cloned()
            .collect();
        self.apply_carry_plan(guild, set, drop);
        let sessions = game_activity::sessionize(&input, &spans, gap_timeout_ms);
        let rows = game_activity::aggregate_guild(&guild.to_string(), &sessions);
        if !spans.is_empty() && !self.guild_carry(guild).is_empty() {
            if rows.is_empty() {
                self.requeue(guild, Vec::new(), spans);
                return Vec::new();
            }
            let (silent_spans, _): (Vec<VoiceSpan>, Vec<VoiceSpan>) = spans
                .into_iter()
                .partition(|span| silent.contains(&span.user_key));
            if !silent_spans.is_empty() {
                self.requeue(guild, Vec::new(), silent_spans);
            }
        }
        rows
    }

    /// Every guild holding buffered deltas, closed spans or open spans, for
    /// the flush loop's per-guild due check.
    #[must_use]
    pub fn guilds(&self) -> Vec<Snowflake> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        let mut guilds: Vec<Snowflake> = inner
            .deltas
            .keys()
            .chain(inner.spans.keys())
            .chain(inner.open.keys().map(|(guild, _)| guild))
            .copied()
            .collect();
        guilds.sort_unstable();
        guilds.dedup();
        guilds
    }

    /// Remove and return one guild's buffered deltas and voice spans at
    /// `now_ms`, clipping each open span there.
    ///
    /// The pre-flush portion of every open span is returned as a closed span
    /// for this window, and the open restarts at `now_ms` so the next window
    /// keeps continuity. Opens already at or past `now_ms` (clock skew) are
    /// left alone. On a persist failure the caller requeues the returned
    /// spans, so the clipped portion is preserved in the buffer while the
    /// opens continue from the clip point: no time is lost and none counts
    /// twice.
    fn drain_guild_at(
        &self,
        guild: Snowflake,
        now_ms: i64,
    ) -> (Vec<PresenceDelta>, Vec<VoiceSpan>) {
        let Ok(mut inner) = self.inner.lock() else {
            return (Vec::new(), Vec::new());
        };
        let deltas = inner.deltas.remove(&guild).unwrap_or_default();
        let mut spans = inner.spans.remove(&guild).unwrap_or_default();
        for ((span_guild, member), open) in inner.open.iter_mut() {
            if *span_guild != guild || now_ms <= open.join_ms {
                continue;
            }
            spans.push(VoiceSpan {
                user_key: member.to_string(),
                join_ms: open.join_ms,
                leave_ms: now_ms,
                self_stream: open.self_stream,
            });
            open.join_ms = now_ms;
        }
        (deltas, spans)
    }

    /// This guild's carried play states, keyed by member key.
    fn guild_carry(&self, guild: Snowflake) -> BTreeMap<String, PlayCarry> {
        let Ok(inner) = self.inner.lock() else {
            return BTreeMap::new();
        };
        inner
            .carry
            .iter()
            .filter(|((carry_guild, _), _)| *carry_guild == guild)
            .map(|((_, user_key), stated)| (user_key.clone(), stated.clone()))
            .collect()
    }

    /// Apply one flush's carry updates: still-playing members reopen from
    /// the flush clock, stopped members (and TTL-stale ones) are dropped.
    fn apply_carry_plan(&self, guild: Snowflake, set: Vec<(String, PlayCarry)>, drop: Vec<String>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        for user_key in drop {
            inner.carry.remove(&(guild, user_key));
        }
        for (user_key, stated) in set {
            inner.carry.insert((guild, user_key), stated);
        }
    }

    /// Forget every carried play state for `guild`: a presence-disabled
    /// flush drains and drops, so a re-enable starts fresh.
    fn clear_guild_carry(&self, guild: Snowflake) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner
            .carry
            .retain(|(carry_guild, _), _| *carry_guild != guild);
    }

    /// Forget the carried play states of opted-out members: their future
    /// deltas are filtered before sessionization, so a stale carry must
    /// never bridge pre-opt-out play into a later window.
    fn drop_carry_users(&self, guild: Snowflake, users: &std::collections::BTreeSet<String>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.carry.retain(|(carry_guild, user_key), _| {
            *carry_guild != guild || !users.contains(user_key)
        });
    }

    /// Push drained buffers back to the front of the guild's queues, oldest
    /// first, evicting past the caps like a fresh observation. The persist
    /// path requeues after an upsert failure so a database outage loses
    /// nothing: the next due tick re-aggregates the same buffers.
    fn requeue(&self, guild: Snowflake, mut deltas: Vec<PresenceDelta>, mut spans: Vec<VoiceSpan>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let overflow = {
            let buffer = inner.deltas.entry(guild).or_default();
            deltas.extend(std::mem::take(buffer));
            let overflow = deltas.len().saturating_sub(MAX_DELTAS_PER_GUILD);
            if overflow > 0 {
                deltas.drain(..overflow);
            }
            *buffer = std::mem::take(&mut deltas);
            overflow
        };
        if overflow > 0 {
            inner.dropped_deltas += overflow as u64;
        }
        let overflow = {
            let buffer = inner.spans.entry(guild).or_default();
            spans.extend(std::mem::take(buffer));
            let overflow = spans.len().saturating_sub(MAX_SPANS_PER_GUILD);
            if overflow > 0 {
                spans.drain(..overflow);
            }
            *buffer = std::mem::take(&mut spans);
            overflow
        };
        if overflow > 0 {
            inner.dropped_spans += overflow as u64;
        }
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

/// One synthetic observation asserting `user_key` still plays `game` at
/// `at_ms`: the materialized form of "no change reported". Carries only the
/// fields [`sessionize`](game_activity::sessionize) reads (type and title);
/// everything else is empty, so a synthetic never invents details, party,
/// assets or URLs.
fn synthetic_delta(user_key: &str, game: &str, is_streaming: bool, at_ms: i64) -> PresenceDelta {
    PresenceDelta {
        user_key: user_key.to_owned(),
        observed_at_ms: at_ms,
        activities: vec![ActivitySnapshot {
            activity_type: if is_streaming {
                CoreActivityType::Streaming
            } else {
                CoreActivityType::Playing
            },
            name: game.to_owned(),
            application_id: None,
            details: None,
            state: None,
            party_size: None,
            assets: false,
            button_labels: Vec::new(),
            button_urls: Vec::new(),
            stream_url: None,
        }],
    }
}

/// Bridge `(from_ms, to_ms)` for one member and game with synthetic ticks no
/// more than `gap_timeout_ms` apart, so [`sessionize`](game_activity::sessionize)
/// merges the span whole instead of splitting it into floored singletons.
/// `to_ms` itself is emitted by the caller (the real delta or the flush
/// clock); a non-positive gap emits nothing rather than looping.
fn fill_ticks(
    out: &mut Vec<PresenceDelta>,
    user_key: &str,
    game: &str,
    is_streaming: bool,
    from_ms: i64,
    to_ms: i64,
    gap_timeout_ms: i64,
) {
    if gap_timeout_ms <= 0 {
        return;
    }
    let mut tick = from_ms.saturating_add(gap_timeout_ms);
    while tick < to_ms {
        out.push(synthetic_delta(user_key, game, is_streaming, tick));
        tick = tick.saturating_add(gap_timeout_ms);
    }
}

/// Plan one guild's flush window: the [`sessionize`](game_activity::sessionize)
/// input plus the carry updates to apply.
///
/// Groups `reals` per member (time order), prepends each member's carried
/// open interval, fills same-game gaps wider than `gap_timeout_ms` — and the
/// run-up to a stop delta — with synthetic ticks, and extends a still-playing
/// tail to `now_ms`, reopening it there for the next window. Returns the
/// sessionize input, the carries to set, and the carries to drop.
///
/// A member with no fresh delta in this window contributes nothing: their
/// carry waits untouched for a later window. A carry whose first fresh
/// observation changed games is dropped rather than floored to a phantom
/// minute (the old game's close was already materialized into the buffer by
/// [`GameActivityCollector::observe_presence`] when the change arrived). A carry with no fresh observation past
/// [`GAME_ACTIVITY_CARRY_TTL_MS`] is dropped, so a much later same-game
/// delta starts a new interval instead of retroactively crediting the gap.
fn plan_flush(
    reals: &[PresenceDelta],
    carry: &BTreeMap<String, PlayCarry>,
    gap_timeout_ms: i64,
    now_ms: i64,
) -> (Vec<PresenceDelta>, Vec<(String, PlayCarry)>, Vec<String>) {
    let mut by_user: BTreeMap<&str, Vec<&PresenceDelta>> = BTreeMap::new();
    for delta in reals {
        by_user
            .entry(delta.user_key.as_str())
            .or_default()
            .push(delta);
    }
    let mut input: Vec<PresenceDelta> = Vec::new();
    let mut set: Vec<(String, PlayCarry)> = Vec::new();
    let mut drop: Vec<String> = Vec::new();
    for (user_key, stated) in carry {
        if !by_user.contains_key(user_key.as_str())
            && stated.from_ms <= now_ms
            && now_ms.saturating_sub(stated.from_ms) > GAME_ACTIVITY_CARRY_TTL_MS
        {
            drop.push(user_key.clone());
        }
    }
    for (user_key, items) in &by_user {
        let mut sequence: Vec<PresenceDelta> = Vec::with_capacity(items.len() + 1);
        let mut ordered: Vec<&PresenceDelta> = items.clone();
        ordered.sort_by_key(|delta| delta.observed_at_ms);
        if let Some(stated) = carry.get(*user_key) {
            if stated.from_ms <= now_ms
                && now_ms.saturating_sub(stated.from_ms) > GAME_ACTIVITY_CARRY_TTL_MS
            {
                drop.push((*user_key).to_owned());
            } else {
                // Reopen the carried interval ahead of the fresh deltas. A
                // carried game met by a different game was already over:
                // skip the reopened point instead of flooring a phantom
                // minute for it. A stop still bridges the run-up to it.
                let bridge = match ordered.first() {
                    Some(first) if stated.from_ms > first.observed_at_ms => false,
                    first => match first.and_then(|delta| game_activity::session_game(delta)) {
                        Some((game, is_streaming)) => {
                            game == stated.game && is_streaming == stated.is_streaming
                        }
                        None => true,
                    },
                };
                if bridge {
                    sequence.push(synthetic_delta(
                        user_key,
                        &stated.game,
                        stated.is_streaming,
                        stated.from_ms.min(now_ms),
                    ));
                }
            }
        }
        sequence.extend(ordered.into_iter().cloned());
        let mut open: Option<(i64, String, bool)> = None;
        for delta in &sequence {
            match game_activity::session_game(delta) {
                Some((game, is_streaming)) => {
                    if let Some((started_ms, open_game, open_streaming)) = open.take() {
                        if game == open_game && is_streaming == open_streaming {
                            fill_ticks(
                                &mut input,
                                user_key,
                                &game,
                                is_streaming,
                                started_ms,
                                delta.observed_at_ms,
                                gap_timeout_ms,
                            );
                        }
                    }
                    open = Some((delta.observed_at_ms, game, is_streaming));
                    input.push(delta.clone());
                }
                None => {
                    // A stop closes the member's interval, but the run-up to
                    // it still counts: presence only re-asserts on change.
                    // The trailing leg needs a playing observation at the
                    // stop clock (mirroring the still-playing tail below),
                    // or the session ends at the last bridge tick instead.
                    if let Some((started_ms, open_game, open_streaming)) = open.take() {
                        fill_ticks(
                            &mut input,
                            user_key,
                            &open_game,
                            open_streaming,
                            started_ms,
                            delta.observed_at_ms,
                            gap_timeout_ms,
                        );
                        if delta.observed_at_ms > started_ms {
                            input.push(synthetic_delta(
                                user_key,
                                &open_game,
                                open_streaming,
                                delta.observed_at_ms,
                            ));
                        }
                    }
                    input.push(delta.clone());
                }
            }
        }
        match open {
            Some((started_ms, open_game, open_streaming)) if now_ms > started_ms => {
                fill_ticks(
                    &mut input,
                    user_key,
                    &open_game,
                    open_streaming,
                    started_ms,
                    now_ms,
                    gap_timeout_ms,
                );
                input.push(synthetic_delta(
                    user_key,
                    &open_game,
                    open_streaming,
                    now_ms,
                ));
                set.push((
                    (*user_key).to_owned(),
                    PlayCarry {
                        game: open_game,
                        is_streaming: open_streaming,
                        from_ms: now_ms,
                        last_ms: now_ms,
                        stopped_at: None,
                        resumed_at: None,
                    },
                ));
            }
            Some((started_ms, open_game, open_streaming)) => {
                // The flush clock runs behind the last observation (skew):
                // carry from the observation, extending nothing.
                set.push((
                    (*user_key).to_owned(),
                    PlayCarry {
                        game: open_game,
                        is_streaming: open_streaming,
                        from_ms: started_ms,
                        last_ms: started_ms,
                        stopped_at: None,
                        resumed_at: None,
                    },
                ));
            }
            None => {
                drop.push((*user_key).to_owned());
            }
        }
    }
    (input, set, drop)
}

/// What one persist attempt did: counts only, never games or members.
///
/// The flush loop logs these counts without labels (see [`persist_guild`]):
/// no game title and no member key ever leaves the process in telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistOutcome {
    /// Rows added to the guild counters, with the session count they carry.
    Persisted { rows: u64, sessions: u64 },
    /// The presence toggle is off for the guild: buffers drained and
    /// dropped, nothing written, so a re-enable starts fresh.
    SkippedDisabled,
    /// Nothing buffered, or every buffered member opted out: nothing written.
    Empty,
}

/// Drain one guild's buffers through the opt-out gate into the store.
///
/// Consults the per-user opt-out plus the guild presence toggle (privacy
/// precedent) before anything persists: a disabled guild drains and drops
/// its buffers unwritten and forgets its carries, so a re-enable starts
/// fresh; opted-out members' deltas, spans and carries are filtered before
/// sessionization so they neither persist nor influence another member's
/// aggregates. The guild voice toggle is independent per `docs/privacy.md`:
/// when it is off, voice spans drain and drop while play aggregates still
/// persist, so `in_voice_ms` stays 0. Gate queries fail closed — a database
/// error leaves the buffers untouched for the next due tick. An upsert or commit
/// failure requeues the drained (bridged) buffers instead, so the outage loses
/// nothing; the clip and carry points already advanced stay advanced, and
/// the requeued spans cover the pre-flush portion, so the retry continues
/// the same intervals without loss or double-count. The upsert itself stays
/// additive and drained buffers never flush twice, so a tick persists each
/// interval at most once.
///
/// The gate reads and the upsert share one serializing transaction: it holds
/// `ROW EXCLUSIVE` on the suppression table from before the first gate read
/// until the upsert commits, so a suppression installed mid-flush either
/// lands before the gate (filtered out of this flush) or waits for the
/// commit (filtered out of the next). Either linearization stores zero for
/// the newly-suppressed member from the opting flush on.
///
/// Telemetry is label-free: counts only, no game or member labels.
pub async fn persist_guild(
    pool: &sqlx::PgPool,
    collector: &GameActivityCollector,
    guild: Snowflake,
    gap_timeout_ms: i64,
    now_ms: i64,
) -> Result<PersistOutcome, GameActivityStoreError> {
    let guild_text = guild.to_string();
    // Serialize the gate reads with the upsert below (`record_event`
    // precedent): the transaction holds ROW EXCLUSIVE on the suppression
    // table from before the first gate read until the upsert commits, so an
    // opt-out's SHARE ROW EXCLUSIVE either lands fully before the gate read
    // or waits for the commit. ROW EXCLUSIVE never blocks other writers, and
    // suppression is the only table this flush shares with the opt-out lock
    // set, so there is no lock-ordering hazard. Gate failures still fail
    // closed (rollback, buffers untouched); an upsert or commit failure
    // requeues as before.
    let mut tx = pool.begin().await?;
    sqlx::query("LOCK TABLE privacy_opt_outs IN ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    if game_activity_store::game_analytics_disabled_in(&mut tx, &guild_text).await? {
        tx.rollback().await?;
        let (deltas, spans) = collector.drain_guild_at(guild, now_ms);
        collector.clear_guild_carry(guild);
        tracing::info!(
            deltas = deltas.len(),
            spans = spans.len(),
            "game_activity_flush_skipped"
        );
        return Ok(PersistOutcome::SkippedDisabled);
    }
    let opted_out = game_activity_store::opted_out_user_keys_in(&mut tx, &guild_text).await?;
    let voice_off = game_activity_store::voice_analytics_disabled_in(&mut tx, &guild_text).await?;
    let (deltas, spans) = collector.drain_guild_at(guild, now_ms);
    if !opted_out.is_empty() {
        collector.drop_carry_users(guild, &opted_out);
    }
    let (deltas, spans) = game_activity_store::filter_opted_out(&deltas, &spans, &opted_out);
    let carry = collector.guild_carry(guild);
    let (input, set, drop) = plan_flush(&deltas, &carry, gap_timeout_ms, now_ms);
    // Same silent-member set as [`GameActivityCollector::flush`]: carried
    // members with no fresh delta whose spans survive a non-empty flush.
    let fresh: std::collections::BTreeSet<&str> =
        deltas.iter().map(|delta| delta.user_key.as_str()).collect();
    let dropped: std::collections::BTreeSet<&str> = drop.iter().map(String::as_str).collect();
    let silent: std::collections::BTreeSet<String> = carry
        .keys()
        .filter(|user| !fresh.contains(user.as_str()) && !dropped.contains(user.as_str()))
        .cloned()
        .collect();
    collector.apply_carry_plan(guild, set, drop);
    // A disabled voice family drops its spans after the drain: the opens
    // already restarted at the flush clock, so disabled-period voice is
    // forgotten and `in_voice_ms` stays 0 while play still persists.
    let spans = if voice_off { Vec::new() } else { spans };
    let sessions = game_activity::sessionize(&input, &spans, gap_timeout_ms);
    let rows = game_activity::aggregate_guild(&guild_text, &sessions);
    if rows.is_empty() {
        tracing::debug!(
            deltas = input.len(),
            spans = spans.len(),
            "game_activity_flush_empty"
        );
        // No writes to commit: queue rollback on drop so a cleanup error
        // cannot bypass retention of the already-drained voice spans.
        // Fully qualified: `drop` here names the carry-drop list from
        // `plan_flush`, shadowing `std::mem::drop`.
        std::mem::drop(tx);
        // Same span retention as [`GameActivityCollector::flush`]: with play
        // still carried, drained spans wait for the bridged session instead
        // of dropping covered voice time. Voice-off drops them above.
        if !voice_off && !spans.is_empty() && !collector.guild_carry(guild).is_empty() {
            collector.requeue(guild, Vec::new(), spans);
        }
        return Ok(PersistOutcome::Empty);
    }
    let session_total: u64 = rows.iter().map(|row| row.session_count).sum();
    let persisted: Result<u64, GameActivityStoreError> = async {
        let written = game_activity_store::upsert_guild_aggregates_in(&mut tx, &rows).await?;
        tx.commit().await?;
        Ok(written)
    }
    .await;
    match persisted {
        Ok(written) => {
            // Busy-guild case (see `flush`): retain silent carried members'
            // spans even though other members persisted rows. Retention runs
            // only on success so a failed upsert or commit requeues the full
            // drained buffers below without double-queueing the silent subset.
            if !voice_off && !spans.is_empty() && !collector.guild_carry(guild).is_empty() {
                let (silent_spans, _): (Vec<VoiceSpan>, Vec<VoiceSpan>) = spans
                    .into_iter()
                    .partition(|span| silent.contains(&span.user_key));
                if !silent_spans.is_empty() {
                    collector.requeue(guild, Vec::new(), silent_spans);
                }
            }
            tracing::info!(
                rows = written,
                sessions = session_total,
                "game_activity_flushed"
            );
            Ok(PersistOutcome::Persisted {
                rows: written,
                sessions: session_total,
            })
        }
        Err(error) => {
            collector.requeue(guild, input, spans);
            tracing::warn!("game_activity_persist_failed");
            Err(error)
        }
    }
}

/// Persist every due guild in `collector`, tracking per-guild last-flush
/// times in `last_flush_ms`. A guild seen for the first time stamps `now_ms`
/// and waits a full interval, so a fresh boot never flushes partial minutes.
pub async fn flush_due_guilds(
    pool: &sqlx::PgPool,
    collector: &GameActivityCollector,
    last_flush_ms: &mut HashMap<Snowflake, i64>,
    now_ms: i64,
    gap_timeout_ms: i64,
) {
    for guild in collector.guilds() {
        let last = last_flush_ms.get(&guild).copied();
        if last.is_none() {
            last_flush_ms.insert(guild, now_ms);
            continue;
        }
        if !flush_due(last, now_ms) {
            continue;
        }
        // The buffers drain whether or not the write lands: a disabled guild
        // drops stale state, and a failed upsert requeues inside the call.
        // Only a gate failure (fail-closed, buffers intact) skips the stamp
        // so the next tick retries instead of dropping an hour.
        if persist_guild(pool, collector, guild, gap_timeout_ms, now_ms)
            .await
            .is_ok()
        {
            last_flush_ms.insert(guild, now_ms);
        }
    }
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// Hourly per-guild persist loop for the voice runtime's collector.
///
/// Wakes every `FLUSH_POLL_INTERVAL` and persists each guild due per
/// [`flush_due`]. Detached on purpose: the voice sink owns no shutdown
/// handle, and each tick persists at most once per interval, so a deploy
/// boundary at worst drops the current partial interval — the next tick
/// continues from drained buffers, never double-counting.
pub async fn flush_loop(pool: sqlx::PgPool, collector: Arc<GameActivityCollector>) {
    let mut last_flush_ms: HashMap<Snowflake, i64> = HashMap::new();
    let mut tick = tokio::time::interval(FLUSH_POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        flush_due_guilds(
            &pool,
            &collector,
            &mut last_flush_ms,
            now_ms(),
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
        )
        .await;
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
        let rows = collector.flush(7, 60 * MIN, T0 + 30 * MIN);
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
        let rows = collector.flush(7, 15 * MIN, T0 + 10 * MIN);
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
        let rows = collector.flush(7, 15 * MIN, T0);
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
        let rows = collector.flush(7, 60 * MIN, T0 + 30 * MIN);
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
        let rows = collector.flush(7, 15 * MIN, T0);
        assert_eq!(rows[0].in_voice_ms, 0);
        // Overlapping self-stream span counts like any span.
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        collector.observe_join(7, 42, T0 - MIN);
        collector.observe_self_stream(7, 42, true, T0);
        collector.observe_leave(7, 42, T0 + MIN);
        let rows = collector.flush(7, 15 * MIN, T0);
        assert_eq!(rows[0].in_voice_ms, MIN);
    }

    #[test]
    fn flush_due_needs_a_full_hour() {
        assert!(flush_due(None, T0), "never flushed is due");
        assert!(
            !flush_due(Some(T0), T0 + GAME_ACTIVITY_FLUSH_INTERVAL_MS - 1),
            "one millisecond early is not due"
        );
        assert!(
            flush_due(Some(T0), T0 + GAME_ACTIVITY_FLUSH_INTERVAL_MS),
            "exactly one interval is due"
        );
        assert!(
            flush_due(Some(T0), T0 + 2 * GAME_ACTIVITY_FLUSH_INTERVAL_MS),
            "overdue stays due"
        );
        const {
            assert!(
                GAME_ACTIVITY_FLUSH_INTERVAL_MS >= 4 * GAME_ACTIVITY_GAP_TIMEOUT_MS,
                "cadence stays well past the session gap, so steady play sessionizes whole"
            )
        };
    }

    #[test]
    fn guilds_lists_every_guild_with_buffered_or_open_state() {
        let collector = GameActivityCollector::new();
        assert!(collector.guilds().is_empty());
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        collector.observe_join(9, 42, T0);
        assert_eq!(
            collector.guilds(),
            vec![7, 9],
            "deltas and open spans both list"
        );
        collector.observe_leave(9, 42, T0 + MIN);
        assert_eq!(
            collector.guilds(),
            vec![7, 9],
            "closed spans keep their guild listed"
        );
        let _ = collector.flush(7, 15 * MIN, T0);
        let _ = collector.flush(9, 15 * MIN, T0);
        assert_eq!(
            collector.buffered_counts(7),
            (0, 0, 0),
            "flush drains what guilds listed"
        );
    }

    #[test]
    fn requeue_restores_drained_buffers_oldest_first() {
        let collector = GameActivityCollector::new();
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0 + 30 * MIN),
        );
        // Complete the interval: without a stop, a retry at +90m must extend
        // Apex to that clock rather than preserve a 30-minute total.
        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 30 * MIN));
        collector.observe_join(7, 42, T0);
        collector.observe_leave(7, 42, T0 + MIN);
        // Follow persist_guild's pre-upsert path, not a successful flush:
        // materialize the input and apply the carry plan before requeueing.
        let (deltas, spans) = collector.drain_guild_at(7, T0 + 30 * MIN);
        let (input, set, drop) =
            plan_flush(&deltas, &collector.guild_carry(7), 60 * MIN, T0 + 30 * MIN);
        collector.apply_carry_plan(7, set, drop);
        let sessions = game_activity::sessionize(&input, &spans, 60 * MIN);
        let direct = game_activity::aggregate_guild("7", &sessions);
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].total_play_ms, 30 * MIN);
        assert_eq!(direct[0].in_voice_ms, MIN);
        assert!(collector.guild_carry(7).is_empty(), "the stop closes carry");

        // A different member arrives while the upsert is in flight. Their
        // newer presence and voice span must stay behind the failed input.
        let mut newer = presence_delta(&update(json!([playing_activity("Zelda")])), T0 + 90 * MIN);
        newer.user_key = "99".to_owned();
        collector.observe_presence(7, newer);
        collector.observe_join(7, 99, T0 + 89 * MIN);
        collector.observe_leave(7, 99, T0 + 90 * MIN);
        collector.requeue(7, input, spans);
        {
            let inner = collector.inner.lock().expect("collector lock");
            assert_eq!(
                inner.deltas[&7]
                    .iter()
                    .map(|delta| (
                        delta.user_key.as_str(),
                        delta.observed_at_ms,
                        delta.activities.is_empty()
                    ))
                    .collect::<Vec<_>>(),
                vec![
                    ("42", T0, false), // materialized carry
                    ("42", T0, false), // original start
                    ("42", T0 + 30 * MIN, false),
                    ("42", T0 + 30 * MIN, true),
                    ("99", T0 + 90 * MIN, false),
                ],
                "inspect queue order before sessionize can sort it"
            );
            assert_eq!(
                inner.spans[&7]
                    .iter()
                    .map(|span| (span.user_key.as_str(), span.join_ms, span.leave_ms))
                    .collect::<Vec<_>>(),
                vec![("42", T0, T0 + MIN), ("99", T0 + 89 * MIN, T0 + 90 * MIN)],
                "failed voice evidence precedes the newer member's span"
            );
        }
        let rows = collector.flush(7, 60 * MIN, T0 + 90 * MIN);
        assert_eq!(rows.len(), 2, "both members survive retry, got {rows:?}");
        let apex = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Apex"))
            .expect("requeued Apex row");
        assert_eq!(apex.total_play_ms, 30 * MIN);
        assert_eq!(apex.in_voice_ms, MIN);
        assert_eq!(apex.session_count, 1);
        let zelda = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Zelda"))
            .expect("newer Zelda row");
        assert_eq!(zelda.total_play_ms, MIN, "new sighting keeps its floor");
        assert_eq!(
            zelda.in_voice_ms, 0,
            "voice ended at Zelda's first sighting"
        );
        assert_eq!(zelda.session_count, 1);
        assert_eq!(collector.dropped_counts(), (0, 0));
        assert_eq!(collector.buffered_counts(7), (0, 0, 0));
    }

    #[test]
    fn requeue_continued_play_extends_to_retry_clock() {
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
        let (deltas, spans) = collector.drain_guild_at(7, T0 + 30 * MIN);
        let (input, set, drop) =
            plan_flush(&deltas, &collector.guild_carry(7), 60 * MIN, T0 + 30 * MIN);
        collector.apply_carry_plan(7, set, drop);
        assert_eq!(collector.guild_carry(7)["42"].from_ms, T0 + 30 * MIN);
        collector.requeue(7, input, spans);

        // No stop arrived, and the failed attempt wrote nothing: retry owes
        // all 90 minutes, not just the first 30 or the final 60.
        let rows = collector.flush(7, 60 * MIN, T0 + 90 * MIN);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].game.as_deref(), Some("Apex"));
        assert_eq!(rows[0].total_play_ms, 90 * MIN);
        assert_eq!(rows[0].in_voice_ms, MIN);
        assert_eq!(rows[0].session_count, 1);
        assert_eq!(collector.buffered_counts(7), (0, 0, 0));
        assert!(
            collector.flush(7, 60 * MIN, T0 + 120 * MIN).is_empty(),
            "a successful retry consumes the replay; a silent carry adds no rows"
        );
    }

    #[test]
    fn requeue_overflow_evicts_oldest_and_preserves_new_arrivals() {
        let collector = GameActivityCollector::new();
        let idle = presence_delta(&update(json!([])), T0);
        let deltas = (0..MAX_DELTAS_PER_GUILD)
            .map(|step| PresenceDelta {
                observed_at_ms: T0 + i64::try_from(step).expect("bounded delta index"),
                ..idle.clone()
            })
            .collect();
        let spans = (0..MAX_SPANS_PER_GUILD)
            .map(|step| {
                let join_ms = T0 + i64::try_from(step).expect("bounded span index");
                VoiceSpan {
                    user_key: "42".to_owned(),
                    join_ms,
                    leave_ms: join_ms + 1,
                    self_stream: false,
                }
            })
            .collect();
        let newer_delta_ms = T0 + i64::try_from(MAX_DELTAS_PER_GUILD).expect("delta cap");
        let newer_join_ms = T0 + i64::try_from(MAX_SPANS_PER_GUILD).expect("span cap");
        collector.observe_presence(7, presence_delta(&update(json!([])), newer_delta_ms));
        collector.observe_join(7, 42, newer_join_ms);
        collector.observe_leave(7, 42, newer_join_ms + 1);
        collector.requeue(7, deltas, spans);

        assert_eq!(
            collector.buffered_counts(7),
            (MAX_DELTAS_PER_GUILD, MAX_SPANS_PER_GUILD, 0)
        );
        assert_eq!(collector.dropped_counts(), (1, 1));
        let (deltas, spans) = collector.drain_guild_at(7, newer_join_ms + 1);
        assert_eq!(
            deltas
                .iter()
                .map(|delta| delta.observed_at_ms)
                .collect::<Vec<_>>(),
            ((T0 + 1)..=newer_delta_ms).collect::<Vec<_>>(),
            "only the oldest delta is evicted; the new arrival remains last"
        );
        assert_eq!(
            spans
                .iter()
                .map(|span| (span.join_ms, span.leave_ms))
                .collect::<Vec<_>>(),
            ((T0 + 1)..=newer_join_ms)
                .map(|join_ms| (join_ms, join_ms + 1))
                .collect::<Vec<_>>(),
            "only the oldest span is evicted; the new arrival remains last"
        );
    }

    #[test]
    fn open_voice_span_clipped_at_flush_counts_time() {
        let collector = GameActivityCollector::new();
        // Member 42 plays Apex at T0 and T0+55m while staying in voice the
        // whole hour: the flush clips the open span at the flush clock
        // instead of persisting zero in-voice time.
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0 + 55 * MIN),
        );
        collector.observe_join(7, 42, T0);
        let rows = collector.flush(7, 15 * MIN, T0 + 60 * MIN);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].total_play_ms,
            60 * MIN,
            "sparse play bridges to the flush clock, got {:?}",
            rows[0]
        );
        assert_eq!(
            rows[0].in_voice_ms,
            60 * MIN,
            "open span clipped at flush, got {:?}",
            rows[0]
        );
        assert_eq!(
            collector.buffered_counts(7),
            (0, 0, 1),
            "open span retained across the flush"
        );
        // A later window with no fresh presence persists nothing: the carry
        // waits for the eventual stop instead of flooring a phantom minute.
        let rows = collector.flush(7, 15 * MIN, T0 + 120 * MIN);
        assert!(
            rows.is_empty(),
            "idle window persists nothing, got {rows:?}"
        );
    }

    #[test]
    fn sparse_start_stop_sessionizes_whole_across_flushes() {
        let collector = GameActivityCollector::new();
        // Two hours of play with only a start and a stop frame: change-driven
        // presence sends nothing in between, so the first flush must close
        // the open interval at its clock and the second must run to the stop.
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        let rows = collector.flush(7, 15 * MIN, T0 + 60 * MIN);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].total_play_ms,
            60 * MIN,
            "first hour closes at the flush clock, got {:?}",
            rows[0]
        );
        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 120 * MIN));
        let rows = collector.flush(7, 15 * MIN, T0 + 120 * MIN);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].total_play_ms,
            60 * MIN,
            "second hour runs to the stop, got {:?}",
            rows[0]
        );
    }

    #[test]
    fn game_change_closes_previous_game_within_flush() {
        let collector = GameActivityCollector::new();
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Zelda")])), T0 + 40 * MIN),
        );
        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 70 * MIN));

        let rows = collector.flush(7, 15 * MIN, T0 + 70 * MIN);
        let apex = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Apex"))
            .expect("Apex row");
        assert_eq!(apex.total_play_ms, 40 * MIN);
        let zelda = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Zelda"))
            .expect("Zelda row");
        assert_eq!(zelda.total_play_ms, 30 * MIN);
    }

    #[test]
    fn game_change_closes_previous_carry_across_flushes() {
        let collector = GameActivityCollector::new();
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        let rows = collector.flush(7, 15 * MIN, T0 + 10 * MIN);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].total_play_ms, 10 * MIN);

        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Zelda")])), T0 + 20 * MIN),
        );
        let rows = collector.flush(7, 15 * MIN, T0 + 20 * MIN);
        let apex = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Apex"))
            .expect("unflushed Apex carry is closed at the change");
        assert_eq!(apex.total_play_ms, 10 * MIN);
        let zelda = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Zelda"))
            .expect("Zelda row");
        assert_eq!(zelda.total_play_ms, MIN);
    }

    #[test]
    fn stop_then_different_game_in_one_window_credits_no_phantom_play() {
        // Apex 0-10, idle, Zelda 50-60, then one flush: the stop bounds the
        // old run, so the game-change close-out must end at the stop clock
        // instead of bridging straight across the idle gap.
        let collector = GameActivityCollector::new();
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 10 * MIN));
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Zelda")])), T0 + 50 * MIN),
        );
        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 60 * MIN));

        let rows = collector.flush(7, 15 * MIN, T0 + 60 * MIN);
        let apex = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Apex"))
            .expect("Apex row");
        assert_eq!(
            apex.total_play_ms,
            10 * MIN,
            "the stop bounds the old run, got {apex:?}"
        );
        assert_eq!(apex.session_count, 1, "one stopped run, got {apex:?}");
        let zelda = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Zelda"))
            .expect("Zelda row");
        assert_eq!(zelda.total_play_ms, 10 * MIN, "got {zelda:?}");
        assert_eq!(zelda.session_count, 1, "got {zelda:?}");
    }

    #[test]
    fn stop_then_different_game_across_flush_keeps_old_run_up() {
        // Apex 0-40 with a flush at +30, idle, Zelda 80-90, then a flush at
        // +90: the old game was carried over from the earlier flush, so no
        // start delta precedes the stop in this window's buffer. The
        // game-change close-out must still emit the reopen point so the
        // flush's stop-bridge credits Apex 30-40 instead of dropping it.
        let collector = GameActivityCollector::new();
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        let rows = collector.flush(7, 15 * MIN, T0 + 30 * MIN);
        assert_eq!(rows.len(), 1, "first window stores Apex, got {rows:?}");
        assert_eq!(rows[0].total_play_ms, 30 * MIN, "got {:?}", rows[0]);

        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 40 * MIN));
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Zelda")])), T0 + 80 * MIN),
        );
        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 90 * MIN));

        let rows = collector.flush(7, 15 * MIN, T0 + 90 * MIN);
        let apex = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Apex"))
            .expect("Apex run-up to the stop");
        assert_eq!(
            apex.total_play_ms,
            10 * MIN,
            "carried run-up 30-40 survives the game change, got {apex:?}"
        );
        assert_eq!(apex.session_count, 1, "one stopped run, got {apex:?}");
        let zelda = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Zelda"))
            .expect("Zelda row");
        assert_eq!(zelda.total_play_ms, 10 * MIN, "got {zelda:?}");
        assert_eq!(zelda.session_count, 1, "got {zelda:?}");
    }

    #[test]
    fn stop_restart_then_switch_in_one_window_skips_idle_gap() {
        // Apex 0-10, idle, Apex 50-60, Zelda 60-70, then one flush: the
        // game-change close-out must bridge only the post-resume leg
        // (50-60), not the idle gap back to the earliest reopen point.
        let collector = GameActivityCollector::new();
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 10 * MIN));
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0 + 50 * MIN),
        );
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Zelda")])), T0 + 60 * MIN),
        );
        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 70 * MIN));

        let rows = collector.flush(7, 15 * MIN, T0 + 70 * MIN);
        let apex = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Apex"))
            .expect("Apex row");
        assert_eq!(
            apex.total_play_ms,
            20 * MIN,
            "two runs with no idle gap, got {apex:?}"
        );
        assert_eq!(
            apex.session_count, 2,
            "the stop splits the runs, got {apex:?}"
        );
        let zelda = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Zelda"))
            .expect("Zelda row");
        assert_eq!(zelda.total_play_ms, 10 * MIN, "got {zelda:?}");
        assert_eq!(zelda.session_count, 1, "got {zelda:?}");
    }

    #[test]
    fn switch_back_to_first_game_in_one_window_keeps_both_sessions() {
        // Apex 0-120, Zelda 120-180, Apex 180-240, stop at 240, one flush:
        // the carry from the last switch postdates the window's first delta,
        // so plan_flush must not prepend a duplicate reopen point that cuts
        // Zelda's session and floors phantom singletons for both games.
        let collector = GameActivityCollector::new();
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Zelda")])), T0 + 120 * MIN),
        );
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0 + 180 * MIN),
        );
        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 240 * MIN));

        let rows = collector.flush(7, 15 * MIN, T0 + 240 * MIN);
        let apex = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Apex"))
            .expect("Apex row");
        assert_eq!(
            apex.total_play_ms,
            180 * MIN,
            "two Apex legs with no phantom minute, got {apex:?}"
        );
        assert_eq!(apex.session_count, 2, "two Apex runs, got {apex:?}");
        let zelda = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Zelda"))
            .expect("Zelda row");
        assert_eq!(
            zelda.total_play_ms,
            60 * MIN,
            "one uncut Zelda run, got {zelda:?}"
        );
        assert_eq!(zelda.session_count, 1, "got {zelda:?}");
    }

    #[test]
    fn stop_restart_then_switch_across_flush_skips_idle_gap() {
        // Apex 0-40 with a flush at +30, idle, Apex 70-80, Zelda 80-90, then
        // a flush at +90: the carried run-up (30-40) and the post-resume leg
        // (70-80) count while the idle gap stays uncredited.
        let collector = GameActivityCollector::new();
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        let rows = collector.flush(7, 15 * MIN, T0 + 30 * MIN);
        assert_eq!(rows.len(), 1, "first window stores Apex, got {rows:?}");
        assert_eq!(rows[0].total_play_ms, 30 * MIN, "got {:?}", rows[0]);

        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 40 * MIN));
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0 + 70 * MIN),
        );
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Zelda")])), T0 + 80 * MIN),
        );
        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 90 * MIN));

        let rows = collector.flush(7, 15 * MIN, T0 + 90 * MIN);
        let apex = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Apex"))
            .expect("Apex row");
        assert_eq!(
            apex.total_play_ms,
            20 * MIN,
            "carried run-up plus post-resume leg, got {apex:?}"
        );
        assert_eq!(
            apex.session_count, 2,
            "the stop splits the runs, got {apex:?}"
        );
        let zelda = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Zelda"))
            .expect("Zelda row");
        assert_eq!(zelda.total_play_ms, 10 * MIN, "got {zelda:?}");
        assert_eq!(zelda.session_count, 1, "got {zelda:?}");
    }

    #[test]
    fn stop_then_same_game_in_one_window_bridges_without_phantom_gap() {
        // Apex 0-10, stop, Apex 50-60, then one flush: both runs count while
        // the idle gap stays uncredited.
        let collector = GameActivityCollector::new();
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 10 * MIN));
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0 + 50 * MIN),
        );
        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 60 * MIN));

        let rows = collector.flush(7, 15 * MIN, T0 + 60 * MIN);
        assert_eq!(rows.len(), 1, "one game, got {rows:?}");
        assert_eq!(
            rows[0].total_play_ms,
            20 * MIN,
            "both runs with no idle gap, got {:?}",
            rows[0]
        );
        assert_eq!(
            rows[0].session_count, 2,
            "the stop splits the sessions, got {:?}",
            rows[0]
        );
    }

    #[test]
    fn carries_voice_spans_for_silent_player_when_other_game_flushes() {
        let collector = GameActivityCollector::new();
        // Member 42 plays Apex from T0, in voice throughout. The first flush
        // honestly splits there: 10 minutes of play with 10 of voice.
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        collector.observe_join(7, 42, T0);
        let rows = collector.flush(7, 15 * MIN, T0 + 10 * MIN);
        let apex = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Apex"))
            .expect("Apex row");
        assert_eq!(apex.total_play_ms, 10 * MIN);
        assert_eq!(apex.in_voice_ms, 10 * MIN);

        // Member 99 plays Zelda 15-20m while 42 stays silent (no presence
        // updates, still carried). The flush persists Zelda, and 42's
        // clipped span portion waits instead of dropping.
        let mut other_start =
            presence_delta(&update(json!([playing_activity("Zelda")])), T0 + 15 * MIN);
        other_start.user_key = "99".to_owned();
        collector.observe_presence(7, other_start);
        let mut other_stop = presence_delta(&update(json!([])), T0 + 20 * MIN);
        other_stop.user_key = "99".to_owned();
        collector.observe_presence(7, other_stop);

        let rows = collector.flush(7, 15 * MIN, T0 + 20 * MIN);
        assert!(
            rows.iter().any(|row| row.game.as_deref() == Some("Zelda")),
            "another member's completed session produces rows"
        );
        assert_eq!(
            collector.buffered_counts(7),
            (0, 1, 1),
            "the silent carried member's clipped voice span stays buffered"
        );

        // Member 42 stops at +30m. The bridged 10-30m session overlaps the
        // retained 10-20m span plus the 20-30m one: the full 20 voice minutes.
        collector.observe_presence(7, presence_delta(&update(json!([])), T0 + 30 * MIN));
        collector.observe_leave(7, 42, T0 + 30 * MIN);
        let rows = collector.flush(7, 15 * MIN, T0 + 30 * MIN);
        let apex = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Apex"))
            .expect("Apex row");
        assert_eq!(apex.total_play_ms, 20 * MIN);
        assert_eq!(apex.in_voice_ms, 20 * MIN);
    }

    #[test]
    fn stale_out_of_order_game_change_skips_the_bridge() {
        let collector = GameActivityCollector::new();
        // Apex at T0+10m opens the carry; a stale Zelda frame stamped T0+5m
        // still buffers (sessionize sorts) but never clobbers the newer
        // carry, and the flush refuses to bridge Apex into the older game.
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0 + 10 * MIN),
        );
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Zelda")])), T0 + 5 * MIN),
        );
        let rows = collector.flush(7, 15 * MIN, T0 + 10 * MIN);
        assert_eq!(rows.len(), 2, "both sightings stand alone, got {rows:?}");
        let zelda = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Zelda"))
            .expect("Zelda row");
        assert_eq!(
            zelda.total_play_ms, MIN,
            "stale frame floors, got {zelda:?}"
        );
        let apex = rows
            .iter()
            .find(|row| row.game.as_deref() == Some("Apex"))
            .expect("Apex row");
        assert_eq!(
            apex.total_play_ms, MIN,
            "no reopened point ahead of the older game, got {apex:?}"
        );
    }

    #[test]
    fn stale_carry_starts_a_new_interval() {
        let collector = GameActivityCollector::new();
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        let rows = collector.flush(7, 15 * MIN, T0);
        assert_eq!(rows.len(), 1, "singleton floor opens the carry");
        // A day later with nothing in between, the idle flush ages the carry
        // out instead of persisting anything for it.
        let later = T0 + 25 * 60 * MIN;
        let rows = collector.flush(7, 15 * MIN, later);
        assert!(
            rows.is_empty(),
            "idle window persists nothing, got {rows:?}"
        );
        // The next sighting starts a new interval rather than retroactively
        // crediting the whole silent day.
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), later + 5 * MIN),
        );
        let rows = collector.flush(7, 15 * MIN, later + 5 * MIN);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].total_play_ms, MIN,
            "stale carry must not bridge, got {:?}",
            rows[0]
        );
    }

    #[tokio::test]
    async fn persist_guild_writes_then_reports_empty_with_opt_out_and_toggle_gates() {
        let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
            assert!(
                std::env::var("GITHUB_ACTIONS").is_err(),
                "CI must supply the guarded test database"
            );
            eprintln!("SKIP game-activity persist integration: TWO_TEST_DATABASE_URL is not set");
            return;
        };
        let fixture = two_bot_testsupport::TestDatabase::create(
            &url,
            &sqlx::migrate!("../cutover/migrations"),
        )
        .await
        .expect("create migrated agent-testdb fixture");
        let pool = fixture.pool();
        let collector = GameActivityCollector::new();
        // Guild 7 plays Apex for 30 minutes with a 1-minute voice overlap.
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
        let outcome = persist_guild(pool, &collector, 7, 60 * MIN, T0 + 30 * MIN)
            .await
            .expect("first persist writes");
        assert_eq!(
            outcome,
            PersistOutcome::Persisted {
                rows: 1,
                sessions: 1
            }
        );
        let rows = two_bot_core::game_activity_store::read_guild_aggregates(pool, "7")
            .await
            .expect("read back");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].total_play_ms, 30 * MIN);
        assert_eq!(rows[0].in_voice_ms, MIN);
        // A second persist drains nothing and writes nothing: at most once.
        // The still-playing carry lives in the map, not the buffers, so an
        // idle tick stays empty until fresh presence arrives.
        assert_eq!(
            persist_guild(pool, &collector, 7, 60 * MIN, T0 + 30 * MIN)
                .await
                .expect("second persist"),
            PersistOutcome::Empty
        );
        // Guild 9's only member opted out: filtered before sessionization,
        // so nothing persists and the outcome is empty, not skipped.
        collector.observe_presence(
            9,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ('9', '42')")
            .execute(pool)
            .await
            .expect("suppress pair");
        assert_eq!(
            persist_guild(pool, &collector, 9, 60 * MIN, T0)
                .await
                .expect("opted-out persist"),
            PersistOutcome::Empty
        );
        assert!(
            two_bot_core::game_activity_store::read_guild_aggregates(pool, "9")
                .await
                .expect("opted-out read")
                .is_empty()
        );
        // Guild 11 disabled the presence family: buffers drain and drop.
        collector.observe_presence(
            11,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        sqlx::query(
            "INSERT INTO guild_analytics_settings (guild_id, presence_enabled)
             VALUES ('11', FALSE)",
        )
        .execute(pool)
        .await
        .expect("disable presence");
        assert_eq!(
            persist_guild(pool, &collector, 11, 60 * MIN, T0)
                .await
                .expect("disabled persist"),
            PersistOutcome::SkippedDisabled
        );
        assert!(
            two_bot_core::game_activity_store::read_guild_aggregates(pool, "11")
                .await
                .expect("disabled read")
                .is_empty()
        );
        // Guild 13 disabled only the voice family: play persists while
        // voice-derived in-voice time stays 0 (independent toggles).
        collector.observe_presence(
            13,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        collector.observe_presence(
            13,
            presence_delta(&update(json!([playing_activity("Apex")])), T0 + 30 * MIN),
        );
        collector.observe_join(13, 42, T0);
        collector.observe_leave(13, 42, T0 + MIN);
        sqlx::query(
            "INSERT INTO guild_analytics_settings
                 (guild_id, presence_enabled, activity_enabled, voice_enabled)
             VALUES ('13', TRUE, TRUE, FALSE)",
        )
        .execute(pool)
        .await
        .expect("disable voice");
        assert_eq!(
            persist_guild(pool, &collector, 13, 60 * MIN, T0 + 30 * MIN)
                .await
                .expect("voice-disabled persist"),
            PersistOutcome::Persisted {
                rows: 1,
                sessions: 1
            }
        );
        let rows = two_bot_core::game_activity_store::read_guild_aggregates(pool, "13")
            .await
            .expect("voice-disabled read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].total_play_ms, 30 * MIN, "play still persists");
        assert_eq!(
            rows[0].in_voice_ms, 0,
            "voice toggle off zeroes in-voice time"
        );
    }

    #[tokio::test]
    async fn flush_due_guilds_stamps_first_sight_and_persists_when_due() {
        let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
            assert!(
                std::env::var("GITHUB_ACTIONS").is_err(),
                "CI must supply the guarded test database"
            );
            eprintln!("SKIP game-activity cadence integration: TWO_TEST_DATABASE_URL is not set");
            return;
        };
        let fixture = two_bot_testsupport::TestDatabase::create(
            &url,
            &sqlx::migrate!("../cutover/migrations"),
        )
        .await
        .expect("create migrated agent-testdb fixture");
        let pool = fixture.pool();
        let collector = GameActivityCollector::new();
        collector.observe_presence(
            7,
            presence_delta(&update(json!([playing_activity("Apex")])), T0),
        );
        let mut last = std::collections::HashMap::new();
        flush_due_guilds(pool, &collector, &mut last, T0, 60 * MIN).await;
        assert_eq!(
            last.get(&7),
            Some(&T0),
            "first sight stamps without persisting"
        );
        assert!(
            two_bot_core::game_activity_store::read_guild_aggregates(pool, "7")
                .await
                .expect("nothing persisted yet")
                .is_empty()
        );
        flush_due_guilds(pool, &collector, &mut last, T0 + 59 * MIN, 60 * MIN).await;
        assert_eq!(last.get(&7), Some(&T0), "early tick keeps the stamp");
        flush_due_guilds(
            pool,
            &collector,
            &mut last,
            T0 + GAME_ACTIVITY_FLUSH_INTERVAL_MS,
            60 * MIN,
        )
        .await;
        assert_eq!(
            last.get(&7),
            Some(&(T0 + GAME_ACTIVITY_FLUSH_INTERVAL_MS)),
            "due tick persists and restamps"
        );
        assert_eq!(
            two_bot_core::game_activity_store::read_guild_aggregates(pool, "7")
                .await
                .expect("due tick wrote")
                .len(),
            1
        );
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
        let rows = collector.flush(7, 15 * MIN, T0);
        assert!(!rows.is_empty(), "bounded buffers still sessionize");
    }
}
