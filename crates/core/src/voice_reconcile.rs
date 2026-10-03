//! Read-only voice open-half reconciliation (port of two-bot
//! `src/analytics/voiceReconcile.ts` at `bffccf3`, obligation TOG-11152).
//!
//! A session is two halves: a `voice_session_start` row and a
//! `voice_session_end` row. Restarts, pre-TOG-6122 server leaves and bad end
//! rows orphan halves; this module pairs halves per (guild, member) in time
//! order and recovers a duration wherever the stored rows allow it. What
//! cannot be recovered is returned with an explicit reason — never a silent
//! NULL, never a synthesized timestamp.
//!
//! Read-only by design: the inputs are caller-supplied row slices (see
//! [`voice_halves_from_rows`], which projects [`StoredRow`]s without
//! touching them). Nothing here writes, calls REST, hooks the gateway or
//! runs on a timer. General funnel/attribution/anomaly/dashboard/terminal
//! report runtimes stay intentionally dropped per parity §9.3 — this module
//! is only the integrity check.
//!
//! Two deliberate deltas from the legacy fetch stage:
//!
//! * Rows with no member reach the pairing in legacy only by accident (the
//!   fetch filters them silently). Here the adapter counts them in
//!   [`VoiceFeeds::skipped`] instead of dropping them quietly.
//! * Sort tiebreaks compare member IDs numerically ([`Snowflake`]); legacy
//!   compared decimal strings. Order only, never content.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::funnel::parse_iso_millis;
use crate::handlers::StoredRow;
use crate::voice::parse_voice_end_metadata;
use crate::{EventType, Snowflake};

/// One `voice_session_start` row. `channel` is the visit being credited, with
/// the `channel:` source prefix stripped (a source without the prefix passes
/// through verbatim, as legacy `channelOf`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HalfStart {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    /// ISO-8601 UTC of the join, retained verbatim.
    pub occurred_at: String,
    pub channel: String,
}

/// One `voice_session_end` row, with its metadata already parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct HalfEnd {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    /// ISO-8601 UTC of the leave, retained verbatim.
    pub occurred_at: String,
    /// The channel the session was credited to.
    pub channel: String,
    /// False means the tracker never saw the start — the classic restart loss.
    pub start_known: bool,
    /// The tracker's own record of the start, when the row carries one.
    pub started_at: Option<String>,
    /// Seconds, or `None` when the row never measured one.
    pub duration_seconds: Option<f64>,
}

/// One `member_leave` row: the backstop for pre-TOG-6122 server leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveRow {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    /// ISO-8601 UTC of the leave, retained verbatim.
    pub occurred_at: String,
}

/// How an open half got its duration back. Wire strings match legacy exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionKind {
    #[serde(rename = "restart-gap")]
    RestartGap,
    #[serde(rename = "server-leave")]
    ServerLeave,
    #[serde(rename = "metadata-recompute")]
    MetadataRecompute,
}

impl ResolutionKind {
    /// Legacy wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RestartGap => "restart-gap",
            Self::ServerLeave => "server-leave",
            Self::MetadataRecompute => "metadata-recompute",
        }
    }
}

/// Why an open half stays without a duration. Wire strings match legacy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnresolvableReason {
    #[serde(rename = "no-start-on-file")]
    NoStartOnFile,
    #[serde(rename = "superseded")]
    Superseded,
    #[serde(rename = "still-open")]
    StillOpen,
    #[serde(rename = "bad-end-row")]
    BadEndRow,
}

impl UnresolvableReason {
    /// Legacy wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoStartOnFile => "no-start-on-file",
            Self::Superseded => "superseded",
            Self::StillOpen => "still-open",
            Self::BadEndRow => "bad-end-row",
        }
    }
}

/// An open half that got a duration back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSession {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub channel: String,
    pub start_at: String,
    pub end_at: String,
    pub duration_seconds: i64,
    pub resolution: ResolutionKind,
    /// Set when the arithmetic needed a judgement call (clock-skew clamp, or
    /// a `startKnown` flag whose row carried no usable start).
    pub note: Option<String>,
}

/// An open half that stays without a duration, with its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvableSession {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub channel: String,
    /// `None` for orphan ends, which never saw a start.
    pub start_at: Option<String>,
    /// `None` for still-open starts, which never saw an end.
    pub end_at: Option<String>,
    pub reason: UnresolvableReason,
    /// The human sentence: what happened and what (if anything) to do.
    pub detail: String,
}

/// The pairing outcome. Every open half appears exactly once, with either a
/// duration or a reason; healthy ends are counted, not listed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcileResult {
    /// Open halves that got a duration back.
    pub resolved: Vec<ResolvedSession>,
    /// Open halves that stay duration-less, each with its reason.
    pub unresolvable: Vec<UnresolvableSession>,
    /// Ends that already carried a clean duration. Counted, not listed.
    pub complete: usize,
    /// Rows with an unparseable timestamp. Counted, never paired.
    pub skipped: usize,
}

/// The three feeds the pairing needs, projected from stored rows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VoiceFeeds {
    pub starts: Vec<HalfStart>,
    pub ends: Vec<HalfEnd>,
    pub leaves: Vec<LeaveRow>,
    /// Voice/leave rows with no member. Counted, never paired.
    pub skipped: usize,
}

fn channel_of(source: &str) -> String {
    source.strip_prefix("channel:").unwrap_or(source).to_owned()
}

/// Project the three pairing feeds out of persisted rows. Pure and
/// non-destructive: timestamp spellings, unknown starts and null durations
/// pass through unmodified; only the `channel:` source prefix is stripped.
#[must_use]
pub fn voice_halves_from_rows(rows: &[StoredRow]) -> VoiceFeeds {
    let mut feeds = VoiceFeeds::default();
    for row in rows {
        let Some(member_id) = row.member_id else {
            if matches!(
                row.event_type,
                EventType::VoiceSessionStart | EventType::VoiceSessionEnd | EventType::MemberLeave
            ) {
                feeds.skipped += 1;
            }
            continue;
        };
        match row.event_type {
            EventType::VoiceSessionStart => feeds.starts.push(HalfStart {
                guild_id: row.guild_id,
                member_id,
                occurred_at: row.occurred_at.clone(),
                channel: channel_of(&row.source),
            }),
            EventType::VoiceSessionEnd => {
                let parsed = parse_voice_end_metadata(row.metadata.as_ref());
                let started_at = row
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("startedAt"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                feeds.ends.push(HalfEnd {
                    guild_id: row.guild_id,
                    member_id,
                    occurred_at: row.occurred_at.clone(),
                    channel: channel_of(&row.source),
                    start_known: parsed.start_known,
                    started_at,
                    duration_seconds: parsed.duration_seconds,
                });
            }
            EventType::MemberLeave => feeds.leaves.push(LeaveRow {
                guild_id: row.guild_id,
                member_id,
                occurred_at: row.occurred_at.clone(),
            }),
            _ => {}
        }
    }
    feeds
}

/// A finite, non-negative duration rounded to whole seconds, or `None` when
/// the cell holds no number. Mirrors legacy `usableDuration`.
fn usable_duration(raw: Option<f64>) -> Option<i64> {
    let d = raw?;
    if !d.is_finite() || d < 0.0 || d > i64::MAX as f64 {
        return None;
    }
    Some(d.round() as i64)
}

/// Whole seconds between two instants, rounded like legacy
/// `Math.max(0, Math.round((end - start) / 1000))`. Callers only pass
/// `end_ms >= start_ms` except the clock-skew path, which clamps to zero.
fn duration_secs(end_ms: i64, start_ms: i64) -> i64 {
    ((end_ms - start_ms) as f64 / 1000.0).round().max(0.0) as i64
}

/// Legacy `formatVoiceDurationSeconds`: `90s`, `5m`, `2h05m`.
#[must_use]
pub fn format_voice_duration_seconds(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    format!("{}h{:02}m", m / 60, m % 60)
}

enum WalkEvent<'a> {
    End { at_ms: i64, end: &'a HalfEnd },
    Leave { at_ms: i64, leave: &'a LeaveRow },
    Start { at_ms: i64, start: &'a HalfStart },
}

impl WalkEvent<'_> {
    /// At equal timestamps ends run before leaves before starts, matching the
    /// live adapter (a channel move calls leave-old then join-new at the same
    /// instant, and TOG-6122 closes voice before writing the leave).
    fn order(&self) -> u8 {
        match self {
            Self::End { .. } => 0,
            Self::Leave { .. } => 1,
            Self::Start { .. } => 2,
        }
    }

    fn at_ms(&self) -> i64 {
        match self {
            Self::End { at_ms, .. } | Self::Leave { at_ms, .. } | Self::Start { at_ms, .. } => {
                *at_ms
            }
        }
    }
}

/// Pair every half with its mate. One pass per (guild, member), oldest first.
/// A start only ever pairs FORWARDS with an end or leave at or after it:
/// pairing backwards across a later start would invent causality, so an end
/// older than every start stays orphaned and the start stays open.
#[must_use]
pub fn reconcile_voice_halves(
    starts: &[HalfStart],
    ends: &[HalfEnd],
    leaves: &[LeaveRow],
) -> ReconcileResult {
    let mut result = ReconcileResult::default();
    let mut by_member: HashMap<(Snowflake, Snowflake), Vec<WalkEvent<'_>>> = HashMap::new();

    for s in starts {
        let Some(at_ms) = parse_iso_millis(&s.occurred_at) else {
            result.skipped += 1;
            continue;
        };
        by_member
            .entry((s.guild_id, s.member_id))
            .or_default()
            .push(WalkEvent::Start { at_ms, start: s });
    }
    for e in ends {
        let Some(at_ms) = parse_iso_millis(&e.occurred_at) else {
            result.skipped += 1;
            continue;
        };
        by_member
            .entry((e.guild_id, e.member_id))
            .or_default()
            .push(WalkEvent::End { at_ms, end: e });
    }
    for l in leaves {
        let Some(at_ms) = parse_iso_millis(&l.occurred_at) else {
            result.skipped += 1;
            continue;
        };
        by_member
            .entry((l.guild_id, l.member_id))
            .or_default()
            .push(WalkEvent::Leave { at_ms, leave: l });
    }

    for events in by_member.values() {
        let mut events: Vec<&WalkEvent<'_>> = events.iter().collect();
        events.sort_by(|a, b| a.at_ms().cmp(&b.at_ms()).then(a.order().cmp(&b.order())));
        let mut open: Option<(&HalfStart, i64)> = None;

        for ev in events {
            match *ev {
                WalkEvent::Start { at_ms, start } => {
                    // A second start before any end: the tracker REPLACES (one
                    // channel at a time), so the earlier session's end is gone.
                    // Its close is bounded above by this start but the instant
                    // is unknowable — flag it, never stamp this start's time
                    // as its end.
                    if let Some((prev, prev_ms)) = open {
                        result.unresolvable.push(UnresolvableSession {
                            guild_id: prev.guild_id,
                            member_id: prev.member_id,
                            channel: prev.channel.clone(),
                            start_at: Some(prev.occurred_at.clone()),
                            end_at: Some(start.occurred_at.clone()),
                            reason: UnresolvableReason::Superseded,
                            detail: format!(
                                "session starting {} in {} was replaced by a later start at {} before any end was recorded; it ended sometime before then (at most {}), but the instant is unknowable — not a measurable duration.",
                                prev.occurred_at,
                                prev.channel,
                                start.occurred_at,
                                format_voice_duration_seconds(duration_secs(at_ms, prev_ms)),
                            ),
                        });
                    }
                    open = Some((start, at_ms));
                }
                WalkEvent::Leave { at_ms, leave } => {
                    // Pre-TOG-6122 server leave: no end row, but the leave
                    // proves presence up to its instant, so the open session
                    // closes here with a duration.
                    if let Some((prev, prev_ms)) = open {
                        if prev_ms <= at_ms {
                            result.resolved.push(ResolvedSession {
                                guild_id: prev.guild_id,
                                member_id: prev.member_id,
                                channel: prev.channel.clone(),
                                start_at: prev.occurred_at.clone(),
                                end_at: leave.occurred_at.clone(),
                                duration_seconds: duration_secs(at_ms, prev_ms),
                                resolution: ResolutionKind::ServerLeave,
                                note: None,
                            });
                            open = None;
                        }
                    }
                }
                WalkEvent::End { at_ms, end } => {
                    let clean = usable_duration(end.duration_seconds);
                    let open_usable = open.filter(|(_, prev_ms)| *prev_ms <= at_ms);

                    if end.start_known && clean.is_some() {
                        // The healthy case: both halves seen, duration
                        // measured. Not an open half at all — counted so the
                        // summary proves what was NOT swept.
                        result.complete += 1;
                        if open_usable.is_some() {
                            open = None;
                        }
                        continue;
                    }

                    if end.start_known {
                        // Known start, unusable duration: first try the row's
                        // own `startedAt` (the tracker's record travels with
                        // the end), then the open start row.
                        if let Some(meta_ms) = end.started_at.as_deref().and_then(parse_iso_millis)
                        {
                            let clamped = meta_ms > at_ms;
                            result.resolved.push(ResolvedSession {
                                guild_id: end.guild_id,
                                member_id: end.member_id,
                                channel: end.channel.clone(),
                                start_at: end
                                    .started_at
                                    .clone()
                                    .expect("startedAt parsed"),
                                end_at: end.occurred_at.clone(),
                                duration_seconds: duration_secs(at_ms, meta_ms),
                                resolution: ResolutionKind::MetadataRecompute,
                                note: clamped.then(|| {
                                    "end stamped before the recorded start (clock skew), clamped to 0"
                                        .to_owned()
                                }),
                            });
                            if open_usable.is_some() {
                                open = None;
                            }
                            continue;
                        }
                        if let Some((prev, prev_ms)) = open_usable {
                            // The flag claims the start was seen but the row's
                            // own record of it is unusable; the earlier start
                            // row on file is what saves it — the same shape
                            // as a restart loss, so the same resolution name.
                            result.resolved.push(ResolvedSession {
                                guild_id: prev.guild_id,
                                member_id: prev.member_id,
                                channel: end.channel.clone(),
                                start_at: prev.occurred_at.clone(),
                                end_at: end.occurred_at.clone(),
                                duration_seconds: duration_secs(at_ms, prev_ms),
                                resolution: ResolutionKind::RestartGap,
                                note: Some(
                                    "end claimed startKnown but carried no usable startedAt; duration recomputed from the earlier start row"
                                        .to_owned(),
                                ),
                            });
                            open = None;
                            continue;
                        }
                        result.unresolvable.push(UnresolvableSession {
                            guild_id: end.guild_id,
                            member_id: end.member_id,
                            channel: end.channel.clone(),
                            start_at: end.started_at.clone(),
                            end_at: Some(end.occurred_at.clone()),
                            reason: UnresolvableReason::BadEndRow,
                            detail: format!(
                                "end at {} claims a known start but carries no usable duration and no usable startedAt, and no start row for the member predates it — nothing to recompute from.",
                                end.occurred_at,
                            ),
                        });
                        continue;
                    }

                    // Unknown start: the restart-loss case. The database
                    // still holds the start the tracker forgot, so pair with
                    // the latest open start at or before the end.
                    if let Some((prev, prev_ms)) = open_usable {
                        result.resolved.push(ResolvedSession {
                            guild_id: prev.guild_id,
                            member_id: prev.member_id,
                            channel: end.channel.clone(),
                            start_at: prev.occurred_at.clone(),
                            end_at: end.occurred_at.clone(),
                            duration_seconds: duration_secs(at_ms, prev_ms),
                            resolution: ResolutionKind::RestartGap,
                            note: None,
                        });
                        open = None;
                        continue;
                    }
                    result.unresolvable.push(UnresolvableSession {
                        guild_id: end.guild_id,
                        member_id: end.member_id,
                        channel: end.channel.clone(),
                        start_at: None,
                        end_at: Some(end.occurred_at.clone()),
                        reason: UnresolvableReason::NoStartOnFile,
                        detail: format!(
                            "end at {} in {} has no start row at or before it for this member — the bot was down for the join or the member was already in voice when it connected.",
                            end.occurred_at, end.channel,
                        ),
                    });
                }
            }
        }

        // Whatever is still open after the last event is either live right
        // now or lost without a trace — the report says which it cannot tell
        // apart.
        if let Some((prev, _)) = open {
            result.unresolvable.push(UnresolvableSession {
                guild_id: prev.guild_id,
                member_id: prev.member_id,
                channel: prev.channel.clone(),
                start_at: Some(prev.occurred_at.clone()),
                end_at: None,
                reason: UnresolvableReason::StillOpen,
                detail: format!(
                    "start at {} in {} has no end or leave after it — the member may be in voice right now, or the end was lost while the bot was down. Re-run after the member leaves: a fresh end pairs it as complete, a missing one keeps it here.",
                    prev.occurred_at, prev.channel,
                ),
            });
        }
    }

    // Stable output: oldest first, member as the tiebreak.
    result.resolved.sort_by(|a, b| {
        a.start_at
            .cmp(&b.start_at)
            .then(a.member_id.cmp(&b.member_id))
    });
    result.unresolvable.sort_by(|a, b| {
        a.start_at
            .as_deref()
            .unwrap_or("")
            .cmp(b.start_at.as_deref().unwrap_or(""))
            .then(a.member_id.cmp(&b.member_id))
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(member: Snowflake, at: &str, channel: &str) -> HalfStart {
        HalfStart {
            guild_id: 1,
            member_id: member,
            occurred_at: at.to_owned(),
            channel: channel.to_owned(),
        }
    }

    fn end(
        member: Snowflake,
        at: &str,
        start_known: bool,
        started_at: Option<&str>,
        duration_seconds: Option<f64>,
    ) -> HalfEnd {
        HalfEnd {
            guild_id: 1,
            member_id: member,
            occurred_at: at.to_owned(),
            channel: "ch-a".to_owned(),
            start_known,
            started_at: started_at.map(str::to_owned),
            duration_seconds,
        }
    }

    #[test]
    fn healthy_pair_counts_complete_and_consumes_open_start() {
        let starts = vec![start(7, "2026-09-20T12:00:00.000Z", "ch-a")];
        let ends = vec![end(7, "2026-09-20T12:05:30.000Z", true, None, Some(330.0))];
        let r = reconcile_voice_halves(&starts, &ends, &[]);
        assert_eq!(r.complete, 1);
        assert!(r.resolved.is_empty() && r.unresolvable.is_empty() && r.skipped == 0);
    }

    #[test]
    fn unparseable_timestamps_are_skipped_never_paired() {
        let starts = vec![start(7, "not-a-time", "ch-a")];
        let ends = vec![end(7, "2026-09-20T12:05:30.000Z", false, None, None)];
        let r = reconcile_voice_halves(&starts, &ends, &[]);
        assert_eq!(r.skipped, 1);
        // The orphan end still reports honestly — it just cannot pair.
        assert_eq!(
            r.unresolvable
                .iter()
                .filter(|u| u.reason == UnresolvableReason::NoStartOnFile)
                .count(),
            1
        );
    }

    #[test]
    fn duration_formatter_matches_legacy_spellings() {
        assert_eq!(format_voice_duration_seconds(45), "45s");
        assert_eq!(format_voice_duration_seconds(300), "5m");
        assert_eq!(format_voice_duration_seconds(7500), "2h05m");
        assert_eq!(format_voice_duration_seconds(-3), "0s");
    }

    #[test]
    fn unusable_durations_reject_nan_negative_and_infinite() {
        assert_eq!(usable_duration(Some(60.0)), Some(60));
        assert_eq!(usable_duration(Some(60.6)), Some(61));
        assert_eq!(usable_duration(None), None);
        assert_eq!(usable_duration(Some(-5.0)), None);
        assert_eq!(usable_duration(Some(f64::NAN)), None);
        assert_eq!(usable_duration(Some(f64::INFINITY)), None);
    }
}
