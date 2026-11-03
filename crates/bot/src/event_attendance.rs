//! Event Echoes attendance: the event-window voice-session log and its read.
//!
//! The logger half records one row per (member, channel, joined_at, left_at)
//! from `VOICE_STATE_UPDATE` transitions, but only while a mapped event window
//! is open for the guild. Outside event windows nothing is recorded — never an
//! always-on voice log. A window is `[starts_at - 15 min, ends_at + 15 min]`
//! per mapped event (persisted at `event.upsert` confirm next to the key
//! mapping). Sessions overlapping two windows tag both, via the tag union
//! widened at close time.
//!
//! The read half projects the §4 shape for the keyless
//! `event.attendance.read` verb from the stored rows: positive list only
//! (who was there, never absence), bots excluded from members, bots plus AFK
//! occupants excluded from the peak, below-threshold members included with
//! their minutes, and `final` false until the window plus grace has passed.
//!
//! Hook discipline mirrors `crate::event_voice_channels`: the guild actor
//! loop calls [`record_transition`] serially per transition (ordering
//! preserved), each statement bounded by `REVALIDATE_TIMEOUT_MS` so a slow
//! store cannot hold room handling. Failures and timeouts are debug-logged
//! with the guild only — no member or channel ids on this warm path, per
//! docs/log-volume-guard.md — and never fail the actor loop. A missed open
//! stays missed (the documented join-before-window gap); a missed close
//! finalizes on the member's next transition, clamped so a stale row can
//! never tag a later event (see [`plan_transition`]).

use std::sync::OnceLock;

use serde_json::{json, Value};
use time::OffsetDateTime;
use two_bot_core::Snowflake;

use crate::voice_rooms::{RoomPersistence, REVALIDATE_TIMEOUT_MS};

/// Recording grace on each side of `[starts_at, ends_at]`, in millis.
pub(crate) const ATTENDANCE_GRACE_MS: i64 = 15 * 60 * 1_000;
/// Minutes of in-window voice that mark an attendee. Travels in the payload;
/// members below it stay listed with their minutes.
pub(crate) const ATTENDANCE_THRESHOLD_MINUTES: i64 = 10;
/// Relayed-answer budget for the serialized `result` object, mirroring
/// `voice_snapshot`: the Worker refuses bodies over its 64 KiB bound, so the
/// 512-byte reserve covers the `{"ok","result","request_id"}` envelope.
const MAX_ATTENDANCE_RESULT_BYTES: usize = 64 * 1024 - 512;

/// One mapped event window, in epoch millis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttendanceWindowMs {
    pub event_key: String,
    pub starts_ms: i64,
    pub ends_ms: i64,
}

/// One still-open session row the logger must finalize. The channel is
/// `None` when the stored id is corrupt (TEXT snowflake convention): the row
/// still finalizes, but the name refresh skips it and the stored name holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenRow {
    pub id: i64,
    pub channel_id: Option<Snowflake>,
    pub joined_ms: i64,
    pub event_keys: Vec<String>,
}

/// One row to finalize: the stamp and the widened tag union.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClosePlan {
    pub id: i64,
    pub left_ms: i64,
    pub tag_union: Vec<String>,
}

/// One row to open, tagged with the windows containing the join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenPlan {
    pub tags: Vec<String>,
}

/// The logger's decision for one transition. Empty (no closes, no open)
/// means no write: outside windows with no open state, nothing is recorded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TransitionPlan {
    pub closes: Vec<ClosePlan>,
    pub open: Option<OpenPlan>,
}

fn window_open_ms(starts_ms: i64, ends_ms: i64) -> (i64, i64) {
    (
        starts_ms.saturating_sub(ATTENDANCE_GRACE_MS),
        ends_ms.saturating_add(ATTENDANCE_GRACE_MS),
    )
}

fn contains_ms(starts_ms: i64, ends_ms: i64, instant_ms: i64) -> bool {
    let (open, close) = window_open_ms(starts_ms, ends_ms);
    open <= instant_ms && instant_ms <= close
}

fn overlaps_ms(starts_ms: i64, ends_ms: i64, from_ms: i64, to_ms: i64) -> bool {
    let (open, close) = window_open_ms(starts_ms, ends_ms);
    open <= to_ms && from_ms <= close
}

/// Windows overlapping `[from_ms, to_ms]`, keys sorted and deduplicated.
fn overlapping_keys(windows: &[AttendanceWindowMs], from_ms: i64, to_ms: i64) -> Vec<String> {
    let mut keys: Vec<String> = windows
        .iter()
        .filter(|window| overlaps_ms(window.starts_ms, window.ends_ms, from_ms, to_ms))
        .map(|window| window.event_key.clone())
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Decide the writes for one voice transition. Pure over the stored windows,
/// the member's open rows and the transition, so the tagging rules pin
/// without a database:
///
/// - same-channel frames plan nothing (the caller filters them already);
/// - closes finalize every open row: `left_at` is now when the row opened
///   inside a currently-open window (same-event rejoin), otherwise the row's
///   own `joined_at`, so a stale row a missed close left behind closes as a
///   zero-length span that keeps its original tags and can never inflate a
///   later event's minutes;
/// - the tag union widens to every window overlapping `[joined_at, left_at]`,
///   which tags campers that never transition mid-event when they finally
///   leave during a later window;
/// - opens happen only while a window is open, tagged with exactly the
///   windows containing the join; a leave with no open row writes nothing.
pub(crate) fn plan_transition(
    windows: &[AttendanceWindowMs],
    open_rows: &[OpenRow],
    previous: Option<Snowflake>,
    current: Option<Snowflake>,
    now_ms: i64,
) -> TransitionPlan {
    if previous == current {
        return TransitionPlan::default();
    }
    let open_now: Vec<&AttendanceWindowMs> = windows
        .iter()
        .filter(|window| contains_ms(window.starts_ms, window.ends_ms, now_ms))
        .collect();
    let mut closes = Vec::new();
    for row in open_rows {
        let same_event = open_now
            .iter()
            .any(|window| contains_ms(window.starts_ms, window.ends_ms, row.joined_ms));
        let left_ms = if same_event { now_ms } else { row.joined_ms };
        let mut tag_union = row.event_keys.clone();
        for key in overlapping_keys(windows, row.joined_ms, left_ms) {
            if !tag_union.contains(&key) {
                tag_union.push(key);
            }
        }
        tag_union.sort();
        closes.push(ClosePlan {
            id: row.id,
            left_ms,
            tag_union,
        });
    }
    let open = if current.is_some() && !open_now.is_empty() {
        let mut tags: Vec<String> = open_now
            .iter()
            .map(|window| window.event_key.clone())
            .collect();
        tags.sort();
        tags.dedup();
        Some(OpenPlan { tags })
    } else {
        None
    };
    TransitionPlan { closes, open }
}

/// Whether the attendance logger writes. Resolved once per process from the
/// same env-only flag that gates the read verb: a flip restarts the
/// container, so no per-transition env scan on the voice hot path.
pub(crate) fn logger_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("TWO_INTERNAL_ALLOW_ATTENDANCE_READ").is_ok_and(|value| value == "1")
    })
}

/// Record one voice-channel transition (join, leave or move) as attendance
/// sessions. Called by the guild actor loop before room handling; each
/// statement is bounded by `REVALIDATE_TIMEOUT_MS` (the same inbox-protection
/// bound as creator revalidation) so a slow statement cannot hold room
/// handling. `is_bot` and `afk_channel` arrive from the dispatch (the cache
/// view of the transition); the channel names are the actor's latest live
/// view, refreshed at close so renames converge. Never fails the actor loop.
#[allow(clippy::too_many_arguments)]
pub async fn record_transition(
    store: &impl RoomPersistence,
    guild: Snowflake,
    member: Snowflake,
    previous: Option<Snowflake>,
    current: Option<Snowflake>,
    is_bot: bool,
    afk_channel: Option<Snowflake>,
    previous_name: Option<String>,
    current_name: Option<String>,
    enabled: bool,
) {
    if !enabled || previous == current {
        return;
    }
    let windows = match tokio::time::timeout(
        std::time::Duration::from_millis(REVALIDATE_TIMEOUT_MS),
        store.attendance_windows(guild),
    )
    .await
    {
        Ok(Ok(windows)) => windows,
        Ok(Err(error)) => {
            tracing::debug!(
                guild = guild.to_string(),
                error = %error,
                "attendance windows read failed; transition skipped"
            );
            return;
        }
        Err(_) => {
            tracing::debug!(
                guild = guild.to_string(),
                "attendance windows read timed out; transition skipped"
            );
            return;
        }
    };
    if windows.is_empty() {
        return;
    }
    let window_ms: Vec<AttendanceWindowMs> = windows
        .iter()
        .map(|window| AttendanceWindowMs {
            event_key: window.event_key.clone(),
            starts_ms: millis_of(window.starts_at),
            ends_ms: millis_of(window.ends_at),
        })
        .collect();
    let open_rows = match tokio::time::timeout(
        std::time::Duration::from_millis(REVALIDATE_TIMEOUT_MS),
        store.open_attendance_sessions(guild, member),
    )
    .await
    {
        Ok(Ok(rows)) => rows,
        Ok(Err(error)) => {
            tracing::debug!(
                guild = guild.to_string(),
                error = %error,
                "attendance open rows read failed; transition skipped"
            );
            return;
        }
        Err(_) => {
            tracing::debug!(
                guild = guild.to_string(),
                "attendance open rows read timed out; transition skipped"
            );
            return;
        }
    };
    let rows: Vec<OpenRow> = open_rows
        .iter()
        .map(|row| OpenRow {
            id: row.id,
            channel_id: row.channel_id.parse::<Snowflake>().ok(),
            joined_ms: millis_of(row.joined_at),
            event_keys: row.event_keys.clone(),
        })
        .collect();
    let now = OffsetDateTime::now_utc();
    let plan = plan_transition(&window_ms, &rows, previous, current, millis_of(now));
    if plan.closes.is_empty() && plan.open.is_none() {
        return;
    }
    for close in &plan.closes {
        // The closed row keeps the latest name the logger saw: the live
        // name when the row's channel is the transition's previous channel
        // (rename convergence), the stored name otherwise.
        let live_match = rows
            .iter()
            .find(|row| row.id == close.id)
            .and_then(|row| row.channel_id)
            .zip(previous)
            .is_some_and(|(channel, previous)| channel == previous);
        let name = if live_match {
            previous_name.clone().unwrap_or_default()
        } else {
            open_rows
                .iter()
                .find(|row| row.id == close.id)
                .map(|row| row.channel_name.clone())
                .unwrap_or_default()
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(REVALIDATE_TIMEOUT_MS),
            store.close_attendance_session(
                close.id,
                datetime_of(close.left_ms),
                name,
                &close.tag_union,
            ),
        )
        .await;
        match outcome {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::debug!(
                    guild = guild.to_string(),
                    error = %error,
                    "attendance close write failed; the next transition finalizes it"
                );
            }
            Err(_) => {
                tracing::debug!(
                    guild = guild.to_string(),
                    "attendance close write timed out; the next transition finalizes it"
                );
            }
        }
    }
    if let (Some(channel), Some(open)) = (current, &plan.open) {
        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(REVALIDATE_TIMEOUT_MS),
            store.insert_attendance_session(
                guild,
                member,
                channel,
                current_name.unwrap_or_default(),
                is_bot,
                afk_channel.is_some_and(|afk| afk == channel),
                now,
                &open.tags,
            ),
        )
        .await;
        match outcome {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::debug!(
                    guild = guild.to_string(),
                    error = %error,
                    "attendance open write failed; the session stays unlogged"
                );
            }
            Err(_) => {
                tracing::debug!(
                    guild = guild.to_string(),
                    "attendance open write timed out; the session stays unlogged"
                );
            }
        }
    }
}

pub(crate) fn millis_of(instant: OffsetDateTime) -> i64 {
    instant.unix_timestamp().saturating_mul(1_000) + i64::from(instant.millisecond())
}

pub(crate) fn datetime_of(millis: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

// ---------------------------------------------------------------------------
// Read projection (§4 shape)
// ---------------------------------------------------------------------------

/// One stored session row for the read projection, in epoch millis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionMs {
    pub member_id: Snowflake,
    pub channel_id: Snowflake,
    pub channel_name: String,
    pub is_bot: bool,
    pub afk: bool,
    pub joined_ms: i64,
    pub left_ms: Option<i64>,
}

/// Why the attendance read refuses. Both map to existing refusal codes only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttendanceRefusal {
    /// No mapped window, or a mapped window with no tagged sessions: the
    /// read answers `not_found` (404, not retryable) by construction, which
    /// is also the pre-enable answer since windows only persist after the
    /// logger ships.
    NoRecord,
    /// The projection blew the relayed-answer budget. The receiver answers
    /// `internal` (500, retryable): a later read may be smaller.
    OverBudget,
}

/// Project the §4 attendance shape over stored sessions. Pure over plain
/// rows so the threshold/peak/final math pins without a database.
///
/// Rules: bots leave `members` and `rooms` out entirely; AFK sessions keep
/// their member rows but leave `peak_concurrent` out; below-threshold
/// members stay listed with their minutes; rooms carry the latest name the
/// logger saw (`name_at_close`) for channels with at least one human
/// session; still-open rows clamp at `min(now, window close)`; `final` is
/// true once the window plus grace has passed.
pub(crate) fn project_attendance(
    event_key: &str,
    window: &AttendanceWindowMs,
    sessions: &[SessionMs],
    now_ms: i64,
) -> Result<Value, AttendanceRefusal> {
    let (open_ms, close_ms) = window_open_ms(window.starts_ms, window.ends_ms);
    let human: Vec<&SessionMs> = sessions.iter().filter(|row| !row.is_bot).collect();
    if human.is_empty() {
        return Err(AttendanceRefusal::NoRecord);
    }
    // Per-member aggregation over window-clamped intervals.
    let mut members: std::collections::BTreeMap<Snowflake, (i64, u64, i64)> =
        std::collections::BTreeMap::new();
    // Channel names from the latest-joined human session per channel.
    let mut rooms: std::collections::BTreeMap<Snowflake, (i64, String)> =
        std::collections::BTreeMap::new();
    // Sweep events for the peak: (instant, delta), ends before starts.
    let mut sweep: Vec<(i64, i32)> = Vec::new();
    for row in &human {
        let end_ms = row.left_ms.unwrap_or(now_ms.min(close_ms));
        let from_ms = row.joined_ms.max(open_ms);
        let to_ms = end_ms.min(close_ms);
        let entry = members.entry(row.member_id).or_insert((0, 0, i64::MAX));
        entry.1 += 1;
        entry.2 = entry.2.min(row.joined_ms);
        let room = rooms
            .entry(row.channel_id)
            .or_insert((i64::MIN, String::new()));
        if row.joined_ms >= room.0 {
            *room = (row.joined_ms, row.channel_name.clone());
        }
        if from_ms < to_ms {
            entry.0 += (to_ms - from_ms) / 60_000;
            if !row.afk {
                sweep.push((from_ms, 1));
                sweep.push((to_ms, -1));
            }
        }
    }
    sweep.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut peak_concurrent = 0i64;
    let mut running = 0i64;
    for (_, delta) in sweep {
        running += i64::from(delta);
        peak_concurrent = peak_concurrent.max(running);
    }
    let wire_members: Vec<Value> = members
        .into_iter()
        .map(|(member_id, (minutes, sessions, first_joined_ms))| {
            json!({
                "discord_id": member_id.to_string(),
                "minutes": minutes,
                "sessions": sessions,
                "first_joined_at": two_bot_core::funnel::format_iso_millis(first_joined_ms),
            })
        })
        .collect();
    let wire_rooms: Vec<Value> = rooms
        .into_iter()
        .map(|(channel_id, (_, name))| {
            json!({
                "channel_id": channel_id.to_string(),
                "name_at_close": name,
            })
        })
        .collect();
    let value = json!({
        "v": 1,
        "event_key": event_key,
        "window": {
            "starts_at": two_bot_core::funnel::format_iso_millis(window.starts_ms),
            "ends_at": two_bot_core::funnel::format_iso_millis(window.ends_ms),
        },
        "threshold_minutes": ATTENDANCE_THRESHOLD_MINUTES,
        "peak_concurrent": peak_concurrent,
        "rooms": wire_rooms,
        "members": wire_members,
        "final": now_ms >= close_ms,
    });
    let bytes = serde_json::to_vec(&value).map_err(|_| AttendanceRefusal::OverBudget)?;
    if bytes.len() > MAX_ATTENDANCE_RESULT_BYTES {
        return Err(AttendanceRefusal::OverBudget);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    fn window(event_key: &str, starts_ms: i64, ends_ms: i64) -> AttendanceWindowMs {
        AttendanceWindowMs {
            event_key: event_key.to_owned(),
            starts_ms,
            ends_ms,
        }
    }

    fn row(id: i64, channel: Snowflake, joined_ms: i64, keys: &[&str]) -> OpenRow {
        OpenRow {
            id,
            channel_id: Some(channel),
            joined_ms,
            event_keys: keys.iter().map(|key| (*key).to_owned()).collect(),
        }
    }

    fn session(
        member: Snowflake,
        channel: Snowflake,
        name: &str,
        is_bot: bool,
        afk: bool,
        joined_ms: i64,
        left_ms: Option<i64>,
    ) -> SessionMs {
        SessionMs {
            member_id: member,
            channel_id: channel,
            channel_name: name.to_owned(),
            is_bot,
            afk,
            joined_ms,
            left_ms,
        }
    }

    #[test]
    fn same_channel_frames_plan_nothing() {
        let windows = vec![window("game-night", 0, 60 * MIN)];
        let plan = plan_transition(&windows, &[], Some(7), Some(7), 30 * MIN);
        assert_eq!(plan, TransitionPlan::default());
    }

    #[test]
    fn outside_windows_with_no_state_plans_nothing() {
        let windows = vec![window("game-night", 0, 60 * MIN)];
        // Before the early grace: no open rows, no open window, no write.
        let plan = plan_transition(&windows, &[], None, Some(7), -16 * MIN);
        assert_eq!(plan, TransitionPlan::default());
        let plan = plan_transition(&windows, &[], Some(7), None, 200 * MIN);
        assert_eq!(plan, TransitionPlan::default());
    }

    #[test]
    fn join_inside_window_opens_tagged_with_open_windows() {
        let windows = vec![
            window("early-show", 0, 60 * MIN),
            window("late-show", 30 * MIN, 120 * MIN),
        ];
        // 45 min sits inside both windows (with grace): dual-tag.
        let plan = plan_transition(&windows, &[], None, Some(7), 45 * MIN);
        assert!(plan.closes.is_empty());
        assert_eq!(
            plan.open,
            Some(OpenPlan {
                tags: vec!["early-show".to_owned(), "late-show".to_owned()],
            })
        );
    }

    #[test]
    fn move_closes_previous_and_opens_current() {
        let windows = vec![window("game-night", 0, 60 * MIN)];
        let open = vec![row(1, 7, 10 * MIN, &["game-night"])];
        let plan = plan_transition(&windows, &open, Some(7), Some(9), 20 * MIN);
        assert_eq!(
            plan.closes,
            vec![ClosePlan {
                id: 1,
                left_ms: 20 * MIN,
                tag_union: vec!["game-night".to_owned()],
            }]
        );
        assert_eq!(
            plan.open,
            Some(OpenPlan {
                tags: vec!["game-night".to_owned()],
            })
        );
    }

    #[test]
    fn close_after_window_still_finalizes_without_opening() {
        let windows = vec![window("game-night", 0, 60 * MIN)];
        let open = vec![row(1, 7, 10 * MIN, &["game-night"])];
        // Leave lands after the late grace: the row still closes (left stamps
        // now, inside the window's own span so its tags hold), but with no
        // current channel nothing opens.
        let plan = plan_transition(&windows, &open, Some(7), None, 200 * MIN);
        assert_eq!(
            plan.closes,
            vec![ClosePlan {
                id: 1,
                left_ms: 200 * MIN,
                tag_union: vec!["game-night".to_owned()],
            }]
        );
        assert_eq!(plan.open, None);
    }

    #[test]
    fn stale_row_from_a_closed_window_closes_zero_length() {
        let windows = vec![
            window("old-show", 0, 60 * MIN),
            window("new-show", 1_000 * MIN, 1_060 * MIN),
        ];
        // The missed close from the old show finalizes during the new show:
        // the row closes at its own join (zero length, original tags only),
        // so it can never inflate the new show's minutes.
        let open = vec![row(1, 7, 10 * MIN, &["old-show"])];
        let plan = plan_transition(&windows, &open, Some(7), Some(9), 1_010 * MIN);
        assert_eq!(
            plan.closes,
            vec![ClosePlan {
                id: 1,
                left_ms: 10 * MIN,
                tag_union: vec!["old-show".to_owned()],
            }]
        );
        assert_eq!(
            plan.open,
            Some(OpenPlan {
                tags: vec!["new-show".to_owned()],
            })
        );
    }

    #[test]
    fn close_widens_tags_to_later_overlapping_windows() {
        let windows = vec![
            window("early-show", 0, 60 * MIN),
            window("late-show", 30 * MIN, 120 * MIN),
        ];
        // Joined during the early show only, leaves during the late show: the
        // closed row tags both, since the session overlapped both windows.
        let open = vec![row(1, 7, 10 * MIN, &["early-show"])];
        let plan = plan_transition(&windows, &open, Some(7), None, 90 * MIN);
        assert_eq!(
            plan.closes,
            vec![ClosePlan {
                id: 1,
                left_ms: 90 * MIN,
                tag_union: vec!["early-show".to_owned(), "late-show".to_owned()],
            }]
        );
        assert_eq!(plan.open, None);
    }

    #[test]
    fn leave_with_no_open_row_plans_nothing() {
        let windows = vec![window("game-night", 0, 60 * MIN)];
        let plan = plan_transition(&windows, &[], Some(7), None, 200 * MIN);
        assert_eq!(plan, TransitionPlan::default());
    }

    #[test]
    fn full_shape_counts_peak_and_final() {
        // Window [60, 120] min; grace opens at 45, closes at 135.
        let event = window("game-night", 60 * MIN, 120 * MIN);
        let sessions = vec![
            // 70 minutes in the lobby, two sessions (move out and back).
            session(11, 7, "lobby", false, false, 50 * MIN, Some(90 * MIN)),
            session(11, 7, "lobby", false, false, 95 * MIN, Some(130 * MIN)),
            // 5 minutes only: stays listed, below threshold.
            session(12, 7, "lobby", false, false, 100 * MIN, Some(105 * MIN)),
            // AFK the whole event: listed as a member, out of the peak.
            session(13, 9, "afk-room", false, true, 50 * MIN, Some(130 * MIN)),
            // Bot: out of members, rooms and peak entirely.
            session(99, 7, "lobby", true, false, 50 * MIN, Some(130 * MIN)),
        ];
        let value = project_attendance("game-night", &event, &sessions, 100 * MIN)
            .expect("fits the budget");
        assert_eq!(value["v"], json!(1));
        assert_eq!(value["event_key"], json!("game-night"));
        assert_eq!(value["threshold_minutes"], json!(10));
        assert_eq!(value["final"], json!(false));
        // Lobby overlap: [50,90] and [95,130] clamp to [45,135] → 75 min
        // over 2 sessions, first seen at 50.
        let members = value["members"].as_array().expect("members array");
        assert_eq!(members.len(), 3);
        assert_eq!(members[0]["discord_id"], json!("11"));
        assert_eq!(members[0]["minutes"], json!(75));
        assert_eq!(members[0]["sessions"], json!(2));
        assert!(members[1]["minutes"].as_i64().expect("minutes") < 10);
        // Peak: members 11 and 12 overlap 100-105; the AFK occupant and the
        // bot never count.
        assert_eq!(value["peak_concurrent"], json!(2));
        let rooms = value["rooms"].as_array().expect("rooms array");
        assert_eq!(rooms.len(), 2);
        assert_eq!(
            value["window"]["starts_at"],
            json!("1970-01-01T01:00:00.000Z")
        );
        assert_eq!(
            value["window"]["ends_at"],
            json!("1970-01-01T02:00:00.000Z")
        );
    }

    #[test]
    fn open_rows_clamp_at_now_before_close_and_close_after() {
        let event = window("game-night", 60 * MIN, 120 * MIN);
        let sessions = vec![session(11, 7, "lobby", false, false, 100 * MIN, None)];
        // Mid-window: open row clamps at now.
        let value = project_attendance("game-night", &event, &sessions, 110 * MIN)
            .expect("fits the budget");
        assert_eq!(value["final"], json!(false));
        assert_eq!(value["members"][0]["minutes"], json!(10));
        // After the late grace: open row clamps at the window close, final.
        let value = project_attendance("game-night", &event, &sessions, 200 * MIN)
            .expect("fits the budget");
        assert_eq!(value["final"], json!(true));
        assert_eq!(value["members"][0]["minutes"], json!(35));
    }

    #[test]
    fn no_human_sessions_is_no_record() {
        let event = window("game-night", 60 * MIN, 120 * MIN);
        let sessions = vec![session(
            99,
            7,
            "lobby",
            true,
            false,
            70 * MIN,
            Some(80 * MIN),
        )];
        assert_eq!(
            project_attendance("game-night", &event, &sessions, 200 * MIN),
            Err(AttendanceRefusal::NoRecord)
        );
        assert_eq!(
            project_attendance("game-night", &event, &[], 200 * MIN),
            Err(AttendanceRefusal::NoRecord)
        );
    }

    #[test]
    fn members_sort_by_discord_id_and_rooms_by_channel() {
        let event = window("game-night", 0, 60 * MIN);
        let sessions = vec![
            session(30, 9, "b-room", false, false, 10 * MIN, Some(20 * MIN)),
            session(20, 7, "a-room", false, false, 10 * MIN, Some(20 * MIN)),
        ];
        let value = project_attendance("game-night", &event, &sessions, 200 * MIN)
            .expect("fits the budget");
        let members = value["members"].as_array().expect("members array");
        assert_eq!(members[0]["discord_id"], json!("20"));
        assert_eq!(members[1]["discord_id"], json!("30"));
        let rooms = value["rooms"].as_array().expect("rooms array");
        assert_eq!(rooms[0]["channel_id"], json!("7"));
        assert_eq!(rooms[1]["channel_id"], json!("9"));
    }
}
