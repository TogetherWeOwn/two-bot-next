//! Read-only voice open-half reconciliation (TOG-11152).
//!
//! Port of legacy two-bot `src/analytics/voiceReconcile.ts` (frozen main
//! `bffccf3`, TOG-8289): a voice session is two halves, a
//! `voice_session_start` row and a `voice_session_end` row. Restarts,
//! pre-TOG-6122 server leaves, and bad end rows orphan one half, leaving a
//! NULL where a duration should be. This module pairs halves per
//! (guild, member) in time order and recovers a duration wherever the stored
//! rows allow it; what cannot be recovered is returned with an explicit
//! reason, never a silent NULL.
//!
//! Timestamps compare as instants ([`parse_iso_millis`], the `Date.parse`
//! analogue), never as strings, and malformed timestamps stay skipped, never
//! paired somewhere. Read-only by construction: [`fetch_voice_halves`] only
//! SELECTs; the pairing is pure over caller-supplied rows. There is no repair
//! path (out of scope on TOG-11152).

use serde::Serialize;
use two_bot_core::funnel::{format_iso_millis, parse_iso_millis};

/// One `voice_session_start` row. The channel is the visit being credited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HalfStart {
    pub guild_id: String,
    pub member_id: String,
    /// ISO-8601 UTC of the join, source spelling retained.
    pub occurred_at: String,
    /// Channel id without the `channel:` prefix.
    pub channel: String,
}

/// One `voice_session_end` row, with its metadata already parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct HalfEnd {
    pub guild_id: String,
    pub member_id: String,
    /// ISO-8601 UTC of the leave, source spelling retained.
    pub occurred_at: String,
    /// The channel the session was credited to.
    pub channel: String,
    /// False means the tracker never saw the start: the classic restart loss.
    pub start_known: bool,
    /// The tracker's own record of the start, when the row carries one.
    pub started_at: Option<String>,
    /// Seconds, or None when the row never measured one.
    pub duration_seconds: Option<f64>,
}

/// One `member_leave` row: the backstop for pre-TOG-6122 server leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveRow {
    pub guild_id: String,
    pub member_id: String,
    /// ISO-8601 UTC of the leave, source spelling retained.
    pub occurred_at: String,
}

/// How an open half got its duration back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResolutionKind {
    RestartGap,
    ServerLeave,
    MetadataRecompute,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedSession {
    pub guild_id: String,
    pub member_id: String,
    pub channel: String,
    pub start_at: String,
    pub end_at: String,
    pub duration_seconds: i64,
    pub resolution: ResolutionKind,
    /// Set when the arithmetic needed a judgement call (clock-skew clamp).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Why an open half stays without a duration. Every value has a fix or owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnresolvableReason {
    NoStartOnFile,
    Superseded,
    StillOpen,
    BadEndRow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnresolvableSession {
    pub guild_id: String,
    pub member_id: String,
    pub channel: String,
    /// None for orphan ends, which never saw a start.
    pub start_at: Option<String>,
    /// None for still-open starts, which never saw an end.
    pub end_at: Option<String>,
    pub reason: UnresolvableReason,
    /// The human sentence: what happened and what (if anything) to do.
    pub detail: String,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileResult {
    /// Open halves that got a duration back.
    pub resolved: Vec<ResolvedSession>,
    /// Open halves that stay duration-less, each with its reason.
    pub unresolvable: Vec<UnresolvableSession>,
    /// Ends that already carried a clean duration. Counted, not listed.
    pub complete: usize,
    /// Rows with an unparseable timestamp or no member. Counted, never paired.
    pub skipped: usize,
}

/// `47m`, `2h05m`, `45s`: compact report durations (legacy
/// `formatVoiceDurationSeconds`).
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

/// A finite, non-negative duration, or None when the cell holds no number
/// (legacy `usableDuration`: `Number` coercion, finite, non-negative, round).
fn usable_duration(raw: Option<f64>) -> Option<i64> {
    let d = raw?;
    if !d.is_finite() || d < 0.0 {
        return None;
    }
    let rounded = d.round();
    if rounded >= i64::MAX as f64 {
        return Some(i64::MAX);
    }
    Some(rounded as i64)
}

enum WalkPayload {
    End(HalfEnd),
    Leave(LeaveRow),
    Start(HalfStart),
}

struct WalkEvent {
    at: i64,
    order: u8,
    payload: WalkPayload,
}

/// Pair every half with its mate. One pass per (guild, member), oldest first;
/// at equal timestamps ends run before leaves before starts, matching the live
/// adapter (a channel move calls leave-old then join-new at the same instant).
///
/// A start only ever pairs FORWARDS with an end or leave at or after it.
/// Pairing backwards across a later start would invent causality, so an end
/// older than every start stays orphaned and the start stays open.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn reconcile_voice_halves(
    starts: &[HalfStart],
    ends: &[HalfEnd],
    leaves: &[LeaveRow],
) -> ReconcileResult {
    let mut result = ReconcileResult::default();
    let mut by_member: std::collections::BTreeMap<(String, String), Vec<WalkEvent>> =
        std::collections::BTreeMap::new();
    let mut push = |guild_id: &str, member_id: &str, ev: WalkEvent| {
        by_member
            .entry((guild_id.to_owned(), member_id.to_owned()))
            .or_default()
            .push(ev);
    };

    for s in starts {
        match parse_iso_millis(&s.occurred_at) {
            Some(at) if !s.member_id.is_empty() => push(
                &s.guild_id,
                &s.member_id,
                WalkEvent {
                    at,
                    order: 2,
                    payload: WalkPayload::Start(s.clone()),
                },
            ),
            _ => result.skipped += 1,
        }
    }
    for e in ends {
        match parse_iso_millis(&e.occurred_at) {
            Some(at) if !e.member_id.is_empty() => push(
                &e.guild_id,
                &e.member_id,
                WalkEvent {
                    at,
                    order: 0,
                    payload: WalkPayload::End(e.clone()),
                },
            ),
            _ => result.skipped += 1,
        }
    }
    for l in leaves {
        match parse_iso_millis(&l.occurred_at) {
            Some(at) if !l.member_id.is_empty() => push(
                &l.guild_id,
                &l.member_id,
                WalkEvent {
                    at,
                    order: 1,
                    payload: WalkPayload::Leave(l.clone()),
                },
            ),
            _ => result.skipped += 1,
        }
    }

    for events in by_member.values_mut() {
        events.sort_by(|a, b| a.at.cmp(&b.at).then(a.order.cmp(&b.order)));
        let mut open: Option<(HalfStart, i64)> = None;

        for ev in events.iter() {
            match &ev.payload {
                WalkPayload::Start(start) => {
                    // A second start before any end: the tracker REPLACES (one
                    // channel at a time), so the earlier session's end is gone.
                    // Its close is bounded above by this start but the instant
                    // is unknowable: flag it, never stamp this start's time.
                    if let Some((prev, _)) = open.take() {
                        let bound = ((ev.at - parse_iso_millis(&prev.occurred_at).unwrap_or(ev.at))
                            .max(0) as f64
                            / 1000.0)
                            .round() as i64;
                        result.unresolvable.push(UnresolvableSession {
                            guild_id: prev.guild_id.clone(),
                            member_id: prev.member_id.clone(),
                            channel: prev.channel.clone(),
                            start_at: Some(prev.occurred_at.clone()),
                            end_at: Some(start.occurred_at.clone()),
                            reason: UnresolvableReason::Superseded,
                            detail: format!(
                                "session starting {} in {} was replaced by a later start at {} \
                                 before any end was recorded; it ended sometime before then \
                                 (at most {}), but the instant is unknowable - not a measurable duration.",
                                prev.occurred_at,
                                prev.channel,
                                start.occurred_at,
                                format_voice_duration_seconds(bound),
                            ),
                        });
                    }
                    open = Some((start.clone(), ev.at));
                }
                WalkPayload::Leave(leave) => {
                    // Pre-TOG-6122 server leave: no end row, but the leave
                    // proves presence up to its instant.
                    if let Some((prev, started)) = open.take() {
                        if started <= ev.at {
                            result.resolved.push(ResolvedSession {
                                guild_id: prev.guild_id.clone(),
                                member_id: prev.member_id.clone(),
                                channel: prev.channel.clone(),
                                start_at: prev.occurred_at.clone(),
                                end_at: leave.occurred_at.clone(),
                                duration_seconds: ((ev.at - started).max(0) as f64 / 1000.0).round()
                                    as i64,
                                resolution: ResolutionKind::ServerLeave,
                                note: None,
                            });
                        } else {
                            open = Some((prev, started));
                        }
                    }
                }
                WalkPayload::End(end) => {
                    let clean = usable_duration(end.duration_seconds);
                    let open_usable = match &open {
                        Some((prev, started)) if *started <= ev.at => {
                            Some((prev.clone(), *started))
                        }
                        _ => None,
                    };

                    if end.start_known && clean.is_some() {
                        // The healthy case: both halves seen, duration
                        // measured. Counted so the summary proves what was NOT
                        // swept.
                        result.complete += 1;
                        if open_usable.is_some() {
                            open = None;
                        }
                        continue;
                    }

                    if end.start_known {
                        // Known start, unusable duration: first try the row's
                        // own `startedAt`, then the open start row.
                        if let Some(meta_at) = end.started_at.as_deref().and_then(parse_iso_millis)
                        {
                            let clamped = meta_at > ev.at;
                            result.resolved.push(ResolvedSession {
                                guild_id: end.guild_id.clone(),
                                member_id: end.member_id.clone(),
                                channel: end.channel.clone(),
                                start_at: end.started_at.clone().unwrap_or_default(),
                                end_at: end.occurred_at.clone(),
                                duration_seconds: ((ev.at - meta_at).max(0) as f64 / 1000.0)
                                    .round()
                                    as i64,
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
                        if let Some((prev, started)) = open_usable {
                            // The flag claims the start was seen but the row's
                            // own record of it is unusable; the earlier start
                            // row on file is what saves it.
                            result.resolved.push(ResolvedSession {
                                guild_id: prev.guild_id.clone(),
                                member_id: prev.member_id.clone(),
                                channel: end.channel.clone(),
                                start_at: prev.occurred_at.clone(),
                                end_at: end.occurred_at.clone(),
                                duration_seconds: ((ev.at - started).max(0) as f64 / 1000.0).round()
                                    as i64,
                                resolution: ResolutionKind::RestartGap,
                                note: Some(
                                    "end claimed startKnown but carried no usable startedAt; \
                                     duration recomputed from the earlier start row"
                                        .to_owned(),
                                ),
                            });
                            open = None;
                            continue;
                        }
                        result.unresolvable.push(UnresolvableSession {
                            guild_id: end.guild_id.clone(),
                            member_id: end.member_id.clone(),
                            channel: end.channel.clone(),
                            start_at: end.started_at.clone(),
                            end_at: Some(end.occurred_at.clone()),
                            reason: UnresolvableReason::BadEndRow,
                            detail: format!(
                                "end at {} claims a known start but carries no usable duration \
                                 and no usable startedAt, and no start row for the member \
                                 predates it - nothing to recompute from.",
                                end.occurred_at,
                            ),
                        });
                        continue;
                    }

                    // Unknown start: the restart-loss case. Pair with the
                    // latest open start at or before the end.
                    if let Some((prev, started)) = open_usable {
                        result.resolved.push(ResolvedSession {
                            guild_id: prev.guild_id.clone(),
                            member_id: prev.member_id.clone(),
                            channel: end.channel.clone(),
                            start_at: prev.occurred_at.clone(),
                            end_at: end.occurred_at.clone(),
                            duration_seconds: ((ev.at - started).max(0) as f64 / 1000.0).round()
                                as i64,
                            resolution: ResolutionKind::RestartGap,
                            note: None,
                        });
                        open = None;
                        continue;
                    }
                    result.unresolvable.push(UnresolvableSession {
                        guild_id: end.guild_id.clone(),
                        member_id: end.member_id.clone(),
                        channel: end.channel.clone(),
                        start_at: None,
                        end_at: Some(end.occurred_at.clone()),
                        reason: UnresolvableReason::NoStartOnFile,
                        detail: format!(
                            "end at {} in {} has no start row at or before it for this member - \
                             the bot was down for the join or the member was already in voice \
                             when it connected. Unmeasurable by construction (docs/EVENTS.md limit 5).",
                            end.occurred_at, end.channel,
                        ),
                    });
                }
            }
        }

        // Whatever is still open after the last event is either live right now
        // or lost without a trace: the report says which it cannot tell apart.
        if let Some((prev, _)) = open {
            result.unresolvable.push(UnresolvableSession {
                guild_id: prev.guild_id.clone(),
                member_id: prev.member_id.clone(),
                channel: prev.channel.clone(),
                start_at: Some(prev.occurred_at.clone()),
                end_at: None,
                reason: UnresolvableReason::StillOpen,
                detail: format!(
                    "start at {} in {} has no end or leave after it - the member may be in \
                     voice right now, or the end was lost while the bot was down. Re-run \
                     after the member leaves: a fresh end pairs it as complete, a missing \
                     one keeps it here.",
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
    });
    result
}

fn channel_of(source: &str) -> String {
    source.strip_prefix("channel:").unwrap_or(source).to_owned()
}

/// Parse one end metadata blob. Unreadable rows are known with no duration:
/// an unreadable row is not evidence of an unknown start (legacy
/// `parseEndMeta`, same rule as `parseVoiceEndMetadata`).
fn parse_end_meta(metadata: Option<&str>) -> (bool, Option<String>, Option<f64>) {
    let value: serde_json::Value =
        serde_json::from_str(metadata.unwrap_or("{}")).unwrap_or(serde_json::Value::Null);
    let start_known = value.get("startKnown").and_then(serde_json::Value::as_bool) != Some(false);
    let started_at = value
        .get("startedAt")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let duration_seconds = match value.get("durationSeconds") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        // Legacy `Number()` coercion on strings: surrounding whitespace is
        // ignored, and empty/whitespace-only is 0, not missing.
        Some(serde_json::Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Some(0.0)
            } else {
                trimmed.parse::<f64>().ok()
            }
        }
        // Legacy `Number(boolean)`: true is 1, false is 0.
        Some(serde_json::Value::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    };
    (start_known, started_at, duration_seconds)
}

/// The three feeds the pairing needs. SELECT only: the sweep never writes.
/// `since` bounds all three feeds (instant comparison); omit it for the
/// full-history sweep. `guild` scopes to one server; omit it for every guild
/// (legacy full sweep reads all guilds).
pub struct VoiceHalves {
    pub starts: Vec<HalfStart>,
    pub ends: Vec<HalfEnd>,
    pub leaves: Vec<LeaveRow>,
}

pub async fn fetch_voice_halves(
    pool: &sqlx::PgPool,
    guild: Option<&str>,
    since: Option<&str>,
) -> Result<VoiceHalves, sqlx::Error> {
    let start_rows: Vec<(String, Option<String>, time::OffsetDateTime, String)> = sqlx::query_as(
        "SELECT guild_id, member_id, occurred_at, source FROM events
          WHERE event_type = 'voice_session_start'
            AND ($1::text IS NULL OR guild_id = $1)
            AND ($2::timestamptz IS NULL OR occurred_at >= $2::timestamptz)
          ORDER BY occurred_at",
    )
    .bind(guild)
    .bind(since)
    .fetch_all(pool)
    .await?;
    let end_rows: Vec<(
        String,
        Option<String>,
        time::OffsetDateTime,
        String,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT guild_id, member_id, occurred_at, source, metadata FROM events
          WHERE event_type = 'voice_session_end'
            AND ($1::text IS NULL OR guild_id = $1)
            AND ($2::timestamptz IS NULL OR occurred_at >= $2::timestamptz)
          ORDER BY occurred_at",
    )
    .bind(guild)
    .bind(since)
    .fetch_all(pool)
    .await?;
    let leave_rows: Vec<(String, Option<String>, time::OffsetDateTime)> = sqlx::query_as(
        "SELECT guild_id, member_id, occurred_at FROM events
          WHERE event_type = 'member_leave'
            AND ($1::text IS NULL OR guild_id = $1)
            AND ($2::timestamptz IS NULL OR occurred_at >= $2::timestamptz)
          ORDER BY occurred_at",
    )
    .bind(guild)
    .bind(since)
    .fetch_all(pool)
    .await?;

    let iso =
        |t: &time::OffsetDateTime| format_iso_millis((t.unix_timestamp_nanos() / 1_000_000) as i64);
    // Memberless rows never pair (legacy drops them at the read, uncounted).
    let member = |m: Option<String>| m.filter(|m| !m.is_empty());
    Ok(VoiceHalves {
        starts: start_rows
            .into_iter()
            .filter_map(|(g, m, at, source)| {
                member(m).map(|member_id| HalfStart {
                    guild_id: g,
                    member_id,
                    occurred_at: iso(&at),
                    channel: channel_of(&source),
                })
            })
            .collect(),
        ends: end_rows
            .into_iter()
            .filter_map(|(g, m, at, source, metadata)| {
                member(m).map(|member_id| {
                    let (start_known, started_at, duration_seconds) =
                        parse_end_meta(metadata.as_deref());
                    HalfEnd {
                        guild_id: g,
                        member_id,
                        occurred_at: iso(&at),
                        channel: channel_of(&source),
                        start_known,
                        started_at,
                        duration_seconds,
                    }
                })
            })
            .collect(),
        leaves: leave_rows
            .into_iter()
            .filter_map(|(g, m, at)| {
                member(m).map(|member_id| LeaveRow {
                    guild_id: g,
                    member_id,
                    occurred_at: iso(&at),
                })
            })
            .collect(),
    })
}

/// Reviewer fixture: seven sessions covering every path, relative to `now_ms`
/// so they always land inside the window. Three resolve (restart-gap,
/// server-leave, metadata-recompute), three stay flagged (still-open,
/// superseded, no-start-on-file), two more ends are healthy completes
/// (legacy `buildSeedHalves`).
pub fn build_seed_halves(now_ms: i64) -> VoiceHalves {
    const HOUR: i64 = 3_600_000;
    const MIN: i64 = 60_000;
    let g = "seed-guild";
    let at = |offset: i64| format_iso_millis(now_ms + offset);
    let start = |member: &str, offset: i64, channel: &str| HalfStart {
        guild_id: g.to_owned(),
        member_id: member.to_owned(),
        occurred_at: at(offset),
        channel: channel.to_owned(),
    };
    let end = |member: &str,
               offset: i64,
               channel: &str,
               start_known: bool,
               started_at: Option<i64>,
               duration_seconds: Option<f64>| HalfEnd {
        guild_id: g.to_owned(),
        member_id: member.to_owned(),
        occurred_at: at(offset),
        channel: channel.to_owned(),
        start_known,
        started_at: started_at.map(|o| at(o)),
        duration_seconds,
    };
    VoiceHalves {
        starts: vec![
            // m1: healthy pair, ends below as a complete (not listed).
            start("m1", -5 * HOUR, "ch-a"),
            // m2: restart loss: the tracker forgot this start.
            start("m2", -4 * HOUR, "ch-a"),
            // m3: pre-TOG-6122 server leave: no end row at all.
            start("m3", -2 * HOUR, "ch-a"),
            // m4: nothing after this start: still open (or lost).
            start("m4", -1 * HOUR, "ch-b"),
            // m5: two starts, one end: the first start is superseded.
            start("m5", -50 * MIN, "ch-a"),
            start("m5", -40 * MIN, "ch-b"),
            // m7: the end below carries a null duration but a valid
            // startedAt, so the duration recomputes from the row itself.
            start("m7", -90 * MIN, "ch-a"),
        ],
        ends: vec![
            end(
                "m1",
                -5 * HOUR + 1_800_000,
                "ch-a",
                true,
                Some(-5 * HOUR),
                Some(1800.0),
            ),
            end("m2", -3 * HOUR, "ch-a", false, None, None),
            end("m5", -35 * MIN, "ch-b", true, Some(-40 * MIN), Some(300.0)),
            // m6: unknown end with no start anywhere on file.
            end("m6", -20 * MIN, "ch-a", false, None, None),
            end("m7", -60 * MIN, "ch-a", true, Some(-90 * MIN), None),
        ],
        leaves: vec![LeaveRow {
            guild_id: g.to_owned(),
            member_id: "m3".to_owned(),
            occurred_at: at(-2 * HOUR + 600_000),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: &str = "g1";

    fn start(member: &str, at: &str, channel: &str) -> HalfStart {
        HalfStart {
            guild_id: G.to_owned(),
            member_id: member.to_owned(),
            occurred_at: at.to_owned(),
            channel: channel.to_owned(),
        }
    }

    fn end(
        member: &str,
        at: &str,
        start_known: bool,
        started_at: Option<&str>,
        duration: Option<f64>,
    ) -> HalfEnd {
        HalfEnd {
            guild_id: G.to_owned(),
            member_id: member.to_owned(),
            occurred_at: at.to_owned(),
            channel: "ch-a".to_owned(),
            start_known,
            started_at: started_at.map(str::to_owned),
            duration_seconds: duration,
        }
    }

    fn leave(member: &str, at: &str) -> LeaveRow {
        LeaveRow {
            guild_id: G.to_owned(),
            member_id: member.to_owned(),
            occurred_at: at.to_owned(),
        }
    }

    #[test]
    fn restart_loss_pairs_with_the_start_row_on_file() {
        let r = reconcile_voice_halves(
            &[start("m", "2026-09-20T10:00:00.000Z", "ch-a")],
            &[end("m", "2026-09-20T11:00:00.000Z", false, None, None)],
            &[],
        );
        assert_eq!(r.resolved.len(), 1);
        assert_eq!(
            r.resolved[0],
            ResolvedSession {
                guild_id: G.to_owned(),
                member_id: "m".to_owned(),
                channel: "ch-a".to_owned(),
                start_at: "2026-09-20T10:00:00.000Z".to_owned(),
                end_at: "2026-09-20T11:00:00.000Z".to_owned(),
                duration_seconds: 3600,
                resolution: ResolutionKind::RestartGap,
                note: None,
            }
        );
        assert!(r.unresolvable.is_empty());
    }

    #[test]
    fn server_leave_closes_the_open_session() {
        let r = reconcile_voice_halves(
            &[start("m", "2026-09-20T10:00:00.000Z", "ch-a")],
            &[],
            &[leave("m", "2026-09-20T10:10:00.000Z")],
        );
        assert_eq!(r.resolved.len(), 1);
        assert_eq!(r.resolved[0].resolution, ResolutionKind::ServerLeave);
        assert_eq!(r.resolved[0].duration_seconds, 600);
        assert!(r.unresolvable.is_empty());
    }

    #[test]
    fn leave_older_than_the_open_start_proves_nothing() {
        let r = reconcile_voice_halves(
            &[start("m", "2026-09-20T10:00:00.000Z", "ch-a")],
            &[],
            &[leave("m", "2026-09-20T09:00:00.000Z")],
        );
        assert!(r.resolved.is_empty());
        assert_eq!(r.unresolvable.len(), 1);
        assert_eq!(r.unresolvable[0].reason, UnresolvableReason::StillOpen);
    }

    #[test]
    fn bad_end_row_recomputes_from_started_at() {
        let r = reconcile_voice_halves(
            &[],
            &[end(
                "m",
                "2026-09-20T10:30:00.000Z",
                true,
                Some("2026-09-20T10:00:00.000Z"),
                None,
            )],
            &[],
        );
        assert_eq!(r.resolved.len(), 1);
        assert_eq!(r.resolved[0].resolution, ResolutionKind::MetadataRecompute);
        assert_eq!(r.resolved[0].duration_seconds, 1800);
        assert!(r.unresolvable.is_empty());
    }

    #[test]
    fn negative_duration_falls_back_to_started_at() {
        let r = reconcile_voice_halves(
            &[],
            &[end(
                "m",
                "2026-09-20T10:30:00.000Z",
                true,
                Some("2026-09-20T10:00:00.000Z"),
                Some(-5.0),
            )],
            &[],
        );
        assert_eq!(r.resolved.len(), 1);
        assert_eq!(r.resolved[0].resolution, ResolutionKind::MetadataRecompute);
        assert_eq!(r.resolved[0].duration_seconds, 1800);
    }

    #[test]
    fn end_before_its_recorded_start_clamps_to_zero_with_note() {
        let r = reconcile_voice_halves(
            &[],
            &[end(
                "m",
                "2026-09-20T10:00:00.000Z",
                true,
                Some("2026-09-20T10:05:00.000Z"),
                None,
            )],
            &[],
        );
        assert_eq!(r.resolved.len(), 1);
        assert_eq!(r.resolved[0].duration_seconds, 0);
        assert!(r.resolved[0]
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("clock skew"));
    }

    #[test]
    fn known_start_with_garbage_started_at_falls_back_to_start_on_file() {
        let r = reconcile_voice_halves(
            &[start("m", "2026-09-20T10:00:00.000Z", "ch-a")],
            &[end(
                "m",
                "2026-09-20T11:00:00.000Z",
                true,
                Some("garbage"),
                None,
            )],
            &[],
        );
        assert_eq!(r.resolved.len(), 1);
        assert_eq!(r.resolved[0].resolution, ResolutionKind::RestartGap);
        assert_eq!(r.resolved[0].duration_seconds, 3600);
        assert!(r.resolved[0]
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("no usable startedAt"));
    }

    #[test]
    fn clean_pair_counts_as_complete_and_lists_nothing() {
        let r = reconcile_voice_halves(
            &[start("m", "2026-09-20T10:00:00.000Z", "ch-a")],
            &[end(
                "m",
                "2026-09-20T10:30:00.000Z",
                true,
                Some("2026-09-20T10:00:00.000Z"),
                Some(1800.0),
            )],
            &[],
        );
        assert_eq!(r.complete, 1);
        assert!(r.resolved.is_empty());
        assert!(r.unresolvable.is_empty());
    }

    #[test]
    fn lone_start_stays_still_open_never_zero_filled() {
        let r = reconcile_voice_halves(&[start("m", "2026-09-20T10:00:00.000Z", "ch-a")], &[], &[]);
        assert!(r.resolved.is_empty());
        assert_eq!(r.unresolvable.len(), 1);
        assert_eq!(r.unresolvable[0].reason, UnresolvableReason::StillOpen);
        assert_eq!(r.unresolvable[0].end_at, None);
        assert!(r.unresolvable[0]
            .detail
            .contains("may be in voice right now"));
    }

    #[test]
    fn second_start_supersedes_the_first() {
        let r = reconcile_voice_halves(
            &[
                start("m", "2026-09-20T10:00:00.000Z", "ch-a"),
                start("m", "2026-09-20T11:00:00.000Z", "ch-b"),
            ],
            &[],
            &[],
        );
        let sup = r
            .unresolvable
            .iter()
            .find(|u| u.reason == UnresolvableReason::Superseded);
        assert!(sup.is_some(), "the replaced start is flagged superseded");
        let sup = sup.unwrap();
        assert_eq!(sup.start_at.as_deref(), Some("2026-09-20T10:00:00.000Z"));
        assert_eq!(sup.end_at.as_deref(), Some("2026-09-20T11:00:00.000Z"));
        assert!(sup.detail.contains("unknowable"));
        assert!(r
            .unresolvable
            .iter()
            .any(|u| u.reason == UnresolvableReason::StillOpen));
    }

    #[test]
    fn unknown_end_with_no_start_is_flagged_not_guessed() {
        let r = reconcile_voice_halves(
            &[],
            &[end("m", "2026-09-20T11:00:00.000Z", false, None, None)],
            &[],
        );
        assert!(r.resolved.is_empty());
        assert_eq!(r.unresolvable.len(), 1);
        assert_eq!(r.unresolvable[0].reason, UnresolvableReason::NoStartOnFile);
        assert_eq!(r.unresolvable[0].start_at, None);
    }

    #[test]
    fn known_end_with_nothing_to_recompute_is_a_bad_end_row() {
        let r = reconcile_voice_halves(
            &[],
            &[end("m", "2026-09-20T11:00:00.000Z", true, None, None)],
            &[],
        );
        assert!(r.resolved.is_empty());
        assert_eq!(r.unresolvable.len(), 1);
        assert_eq!(r.unresolvable[0].reason, UnresolvableReason::BadEndRow);
    }

    #[test]
    fn pairing_only_runs_forwards() {
        let r = reconcile_voice_halves(
            &[start("m", "2026-09-20T12:00:00.000Z", "ch-a")],
            &[end("m", "2026-09-20T11:00:00.000Z", false, None, None)],
            &[],
        );
        assert!(r.resolved.is_empty());
        assert_eq!(r.unresolvable.len(), 2);
        assert!(r
            .unresolvable
            .iter()
            .any(|u| u.reason == UnresolvableReason::NoStartOnFile));
        assert!(r
            .unresolvable
            .iter()
            .any(|u| u.reason == UnresolvableReason::StillOpen));
    }

    #[test]
    fn members_never_pair_across_each_other() {
        let r = reconcile_voice_halves(
            &[start("a", "2026-09-20T10:00:00.000Z", "ch-a")],
            &[end("b", "2026-09-20T11:00:00.000Z", false, None, None)],
            &[],
        );
        assert!(r.resolved.is_empty());
        assert_eq!(r.unresolvable.len(), 2);
    }

    #[test]
    fn channel_move_at_one_instant_closes_old_opens_new() {
        let r = reconcile_voice_halves(
            &[
                start("m", "2026-09-20T10:00:00.000Z", "ch-a"),
                start("m", "2026-09-20T11:00:00.000Z", "ch-b"),
            ],
            &[end(
                "m",
                "2026-09-20T11:00:00.000Z",
                true,
                Some("2026-09-20T10:00:00.000Z"),
                Some(3600.0),
            )],
            &[],
        );
        // Ends sort before starts at equal timestamps: the end closes the
        // first session as a complete, the new start stays open.
        assert_eq!(r.complete, 1);
        assert!(r.resolved.is_empty());
        assert_eq!(
            r.unresolvable.iter().map(|u| u.reason).collect::<Vec<_>>(),
            vec![UnresolvableReason::StillOpen],
        );
    }

    #[test]
    fn malformed_timestamps_are_skipped_never_paired() {
        let r = reconcile_voice_halves(
            &[start("m", "garbage", "ch-a")],
            &[end("", "2026-09-20T11:00:00.000Z", false, None, None)],
            &[],
        );
        assert_eq!(r.skipped, 2);
        assert!(r.resolved.is_empty());
        assert!(r.unresolvable.is_empty());
    }

    #[test]
    fn seeded_halves_cover_every_path() {
        let seed = build_seed_halves(
            parse_iso_millis("2026-09-28T12:00:00.000Z").expect("fixture instant"),
        );
        let r = reconcile_voice_halves(&seed.starts, &seed.ends, &seed.leaves);
        let mut resolutions: Vec<_> = r.resolved.iter().map(|s| s.resolution).collect();
        resolutions.sort_by_key(|k| *k as u8);
        assert_eq!(
            resolutions,
            vec![
                ResolutionKind::RestartGap,
                ResolutionKind::ServerLeave,
                ResolutionKind::MetadataRecompute,
            ]
        );
        let mut reasons: Vec<_> = r.unresolvable.iter().map(|u| u.reason).collect();
        reasons.sort_by_key(|k| *k as u8);
        assert_eq!(
            reasons,
            vec![
                UnresolvableReason::NoStartOnFile,
                UnresolvableReason::Superseded,
                UnresolvableReason::StillOpen,
            ]
        );
        assert_eq!(r.complete, 2);
        for s in &r.resolved {
            assert!(s.duration_seconds >= 0);
        }
        for u in &r.unresolvable {
            assert!(!u.detail.is_empty());
        }
    }

    #[test]
    fn unreadable_metadata_is_known_with_no_duration() {
        let (start_known, started_at, duration) = parse_end_meta(Some("not-json{{{"));
        assert!(start_known);
        assert_eq!(started_at, None);
        assert_eq!(duration, None);
    }

    #[test]
    fn channel_prefix_is_stripped() {
        assert_eq!(channel_of("channel:ch-a"), "ch-a");
        assert_eq!(channel_of("plain-source"), "plain-source");
    }

    #[test]
    fn duration_formatter_matches_legacy() {
        assert_eq!(format_voice_duration_seconds(45), "45s");
        assert_eq!(format_voice_duration_seconds(600), "10m");
        assert_eq!(format_voice_duration_seconds(3600), "1h00m");
        assert_eq!(format_voice_duration_seconds(3720), "1h02m");
    }
}
