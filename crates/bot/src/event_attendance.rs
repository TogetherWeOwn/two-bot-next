//! Positive-list v1 projection for one finalized event window. No I/O.

use serde_json::Value;
use two_bot_core::{funnel::format_iso_millis, Snowflake};

pub mod planning;
pub mod read;
pub mod recovery;

const GRACE_US: i64 = 15 * 60 * 1_000_000;
const THRESHOLD_MINUTES: u8 = 10;
const MAX_INPUT_SESSIONS: usize = 4_096;
const MAX_INPUT_NAME_BYTES: usize = 1024 * 1024;
// Reserve 512 bytes of the 64 KiB relay limit for the reply envelope.
const MAX_RESULT_BYTES: usize = 64 * 1024 - 512;

/// A trusted mapping and the same scope on each explicitly tagged observation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AttendanceScope<'a> {
    pub guild_id: Snowflake,
    pub event_key: &'a str,
}

pub struct AttendanceWindowUs<'a> {
    pub scope: AttendanceScope<'a>,
    pub starts_us: i64,
    pub ends_us: i64,
}

/// One event-tagged observation. `left_us` carries any upstream credit cap.
/// This input is deliberately neither serializable nor debug-printable.
#[derive(Clone, Copy)]
pub struct SessionUs<'a> {
    pub scope: AttendanceScope<'a>,
    pub member_id: Snowflake,
    pub channel_id: Snowflake,
    pub channel_name: &'a str,
    pub name_observed_ns: i128,
    pub is_bot: bool,
    pub afk: bool,
    pub joined_us: i64,
    pub left_us: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttendanceRefusal {
    InvalidWindow,
    NotFinal,
    NoRecord,
    OverBudget,
}

#[derive(serde::Serialize)]
struct AttendanceMember {
    discord_id: String,
    minutes: u64,
    sessions: usize,
    first_joined_at: String,
}

#[derive(serde::Serialize)]
struct AttendanceRoom<'a> {
    channel_id: String,
    name_at_close: &'a str,
}

#[derive(serde::Serialize)]
struct AttendanceResultWindow {
    starts_at: String,
    ends_at: String,
}

#[derive(serde::Serialize)]
struct AttendanceResult<'a> {
    v: u8,
    event_key: &'a str,
    window: AttendanceResultWindow,
    threshold_minutes: u8,
    peak_concurrent: i64,
    rooms: Vec<AttendanceRoom<'a>>,
    members: Vec<AttendanceMember>,
    r#final: bool,
}

/// Project already-authorized, privacy-filtered observations; matching tags
/// are a scope check, not authorization. Close is inclusive for finality.
/// Below-threshold humans stay listed; AFK time earns minutes but not peak.
/// An over-budget result is refused whole rather than truncated.
pub fn project_attendance(
    window: &AttendanceWindowUs<'_>,
    sessions: &[SessionUs<'_>],
    now_us: i64,
) -> Result<Value, AttendanceRefusal> {
    let (open_us, close_us) = match (
        window.starts_us.checked_sub(GRACE_US),
        window.ends_us.checked_add(GRACE_US),
    ) {
        (Some(open), Some(close)) if window.starts_us < window.ends_us => (open, close),
        _ => return Err(AttendanceRefusal::InvalidWindow),
    };
    if now_us < close_us {
        return Err(AttendanceRefusal::NotFinal);
    }
    let mut name_lengths = sessions.iter().map(|row| row.channel_name.len());
    if sessions.len() > MAX_INPUT_SESSIONS
        || window.scope.event_key.len() > MAX_RESULT_BYTES
        || name_lengths
            .try_fold(0, usize::checked_add)
            .is_none_or(|bytes| bytes > MAX_INPUT_NAME_BYTES)
    {
        return Err(AttendanceRefusal::OverBudget);
    }

    // Retain only tagged human rows that intersect this window's grace.
    // A zero-length row can still prove a join, but never earns credit.
    let mut observations: Vec<_> = sessions
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            row.scope == window.scope
                && !row.is_bot
                && row.joined_us.max(open_us) <= row.left_us.unwrap_or(close_us).min(close_us)
        })
        .collect();
    if observations.is_empty() {
        return Err(AttendanceRefusal::NoRecord);
    }
    observations.sort_unstable_by_key(|&(index, row)| (row.member_id, index));
    let (members, peak_concurrent) = project_members(&observations, open_us, close_us);

    // Reuse the buffer. Sorting stamps then input positions makes equal
    // timestamps keep the last input observation, as in the donor.
    observations.retain(|(_, row)| !row.channel_name.is_empty());
    observations
        .sort_unstable_by_key(|&(index, row)| (row.channel_id, row.name_observed_ns, index));
    let rooms = observations
        .chunk_by(|a, b| a.1.channel_id == b.1.channel_id)
        .filter_map(|rows| rows.last())
        .map(|(_, row)| AttendanceRoom {
            channel_id: row.channel_id.to_string(),
            name_at_close: row.channel_name,
        })
        .collect();
    let value = serde_json::to_value(AttendanceResult {
        v: 1,
        event_key: window.scope.event_key,
        window: AttendanceResultWindow {
            starts_at: format_iso_millis(window.starts_us.div_euclid(1_000)),
            ends_at: format_iso_millis(window.ends_us.div_euclid(1_000)),
        },
        threshold_minutes: THRESHOLD_MINUTES,
        peak_concurrent,
        rooms,
        members,
        r#final: true,
    })
    .map_err(|_| AttendanceRefusal::OverBudget)?;
    let bytes = serde_json::to_vec(&value).map_err(|_| AttendanceRefusal::OverBudget)?;
    if bytes.len() > MAX_RESULT_BYTES {
        return Err(AttendanceRefusal::OverBudget);
    }
    Ok(value)
}

fn project_members(
    observations: &[(usize, &SessionUs<'_>)],
    open_us: i64,
    close_us: i64,
) -> (Vec<AttendanceMember>, i64) {
    let mut sweep = Vec::new();
    let members = observations
        .chunk_by(|a, b| a.1.member_id == b.1.member_id)
        .map(|rows| {
            let mut intervals = Vec::new();
            let mut non_afk_intervals = Vec::new();
            let mut first_joined_us = i64::MAX;
            for (_, row) in rows {
                first_joined_us = first_joined_us.min(row.joined_us);
                let from = row.joined_us.max(open_us);
                let to = row.left_us.unwrap_or(close_us).min(close_us);
                if from < to {
                    intervals.push((from, to));
                    if !row.afk {
                        non_afk_intervals.push((from, to));
                    }
                }
            }
            let duration_us: u64 = union_intervals(intervals)
                .into_iter()
                .map(|(from, to)| to.abs_diff(from))
                .sum();
            for (from, to) in union_intervals(non_afk_intervals) {
                sweep.push((from, 1));
                sweep.push((to, -1));
            }
            AttendanceMember {
                discord_id: rows[0].1.member_id.to_string(),
                minutes: duration_us / 60_000_000,
                sessions: rows.len(),
                first_joined_at: format_iso_millis(first_joined_us.div_euclid(1_000)),
            }
        })
        .collect();
    // End events sort before starts at the same instant.
    sweep.sort_unstable();
    let mut peak = 0;
    let mut running = 0;
    for (_, delta) in sweep {
        running += delta;
        peak = peak.max(running);
    }
    (members, peak)
}

// A failed move-close can leave overlapping rows. Union per member, with
// separate unions for duration and non-AFK peak eligibility.
fn union_intervals(mut intervals: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    intervals.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (from, to) in intervals {
        match merged.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => merged.push((from, to)),
        }
    }
    merged
}

#[cfg(test)]
mod tests;
