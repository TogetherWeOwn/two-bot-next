//! Chronological membership projection (legacy `src/store/eventStore.ts`).
//!
//! Occurrence determines join attribution; observation determines presence.
//! REST may reconfirm an old join without inventing a new occurrence. All
//! comparisons retain PostgreSQL's microsecond precision, not epoch millis.

use time::{format_description::well_known::Rfc3339, OffsetDateTime, UtcOffset};

use crate::{EventType, FunnelEvent, FunnelStore, RecordOutcome, Snowflake, StoredRow};

/// Read/observation seam for membership-capable funnel stores. Kept separate
/// from `FunnelStore` so a dispatch buffer need not pretend to be a durable
/// membership read model. The reusable contract suite requires both traits.
pub trait MembershipStore: FunnelStore {
    fn membership(&self, guild_id: Snowflake, member_id: Snowflake) -> Option<Membership>;
    fn membership_rows(&self, guild_id: Snowflake, member_id: Snowflake) -> Vec<StoredRow>;
    /// Insert or reconfirm a membership event. Invalid/non-membership hints
    /// are ignored; an existing row's occurrence/source/other metadata stay put.
    fn record_observed(&self, event: FunnelEvent, observed_at: Option<&str>) -> RecordOutcome;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Membership {
    pub joined_at: Option<String>,
    pub join_source: Option<String>,
    pub left_at: Option<String>,
    pub inactive_flagged_at: Option<String>,
}

/// Injectable dispatch clock: same-tick events and backward wall-clock
/// corrections cannot reverse membership observation order.
#[derive(Debug, Default)]
pub struct MembershipClock {
    last_micros: std::sync::Mutex<Option<i128>>,
}

impl MembershipClock {
    pub fn next_at(&self, wall_millis: i64) -> String {
        let mut last = self.last_micros.lock().expect("membership clock");
        let wall = i128::from(wall_millis) * 1000;
        let next = last.map_or(wall, |previous| wall.max(previous + 1));
        *last = Some(next);
        let time = OffsetDateTime::from_unix_timestamp_nanos(next * 1000)
            .expect("supported membership clock range");
        normalize_timestamp(&time.format(&Rfc3339).expect("UTC timestamp"))
            .expect("microsecond timestamp")
    }
}

/// Normalize a database text timestamp to UTC, preserving six fractional
/// digits. Accepts PostgreSQL's space separator and hour-only session offset.
/// Source: <https://docs.rs/time/0.3.55/time/struct.OffsetDateTime.html#method.parse>
pub fn normalize_timestamp(value: &str) -> Option<String> {
    let mut input = value.trim().to_owned();
    if !input.is_ascii() {
        return None;
    }
    if input.as_bytes().get(10) == Some(&b' ') {
        input.replace_range(10..11, "T");
    }
    let zone_start = input
        .char_indices()
        .skip(19)
        .find_map(|(i, c)| (c == '+' || c == '-').then_some(i));
    if let Some(i) = zone_start {
        match input.len() - i {
            3 => input.push_str(":00"),
            5 => input.insert(i + 3, ':'),
            _ => {}
        }
    }
    // The parser truncates after nine fractional digits. Check the original
    // fraction so submicrosecond evidence cannot disappear during parsing.
    if input.as_bytes().get(19) == Some(&b'.')
        && input.as_bytes()[20..]
            .iter()
            .take_while(|digit| digit.is_ascii_digit())
            .skip(6)
            .any(|digit| *digit != b'0')
    {
        return None;
    }
    let utc = OffsetDateTime::parse(&input, &Rfc3339)
        .ok()?
        .checked_to_offset(UtcOffset::UTC)?;
    if !(0..=9999).contains(&utc.year()) || utc.nanosecond() % 1000 != 0 {
        return None;
    }
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
        utc.microsecond()
    ))
}

/// Observation hints deliberately use the legacy UTC 3–6 digit wire shape.
/// Malformed metadata must never outrank an actual gateway occurrence.
pub fn valid_observation(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if !(24..=27).contains(&bytes.len())
        || bytes.get(10) != Some(&b'T')
        || bytes.get(19) != Some(&b'.')
        || bytes.last() != Some(&b'Z')
    {
        return None;
    }
    normalize_timestamp(value)
}

/// A page-start observation cannot precede the returned member's actual join
/// (a member can join while the page request is in flight).
pub fn member_observation(page_started_at: &str, joined_at: &str) -> Option<String> {
    Some(normalize_timestamp(page_started_at)?.max(normalize_timestamp(joined_at)?))
}

/// Whether the event type carries membership presence (join/leave only).
/// Shared with the sqlx adapter so durable duplicate handling skips row
/// locks for events that can never carry an observation.
pub fn is_membership(kind: EventType) -> bool {
    matches!(kind, EventType::MemberJoin | EventType::MemberLeave)
}

fn metadata_observation(row: &StoredRow) -> Option<String> {
    row.metadata
        .as_ref()
        .and_then(|m| m.get("membershipObservedAt"))
        .and_then(serde_json::Value::as_str)
        .and_then(valid_observation)
}

pub(crate) fn observation(row: &StoredRow) -> Option<String> {
    metadata_observation(row).or_else(|| normalize_timestamp(&row.occurred_at))
}

/// Monotonic duplicate-hint maximum over persisted metadata. Shared with the
/// sqlx adapter so durable reconfirmations converge exactly like memory.
pub fn advance_observation(row: &mut StoredRow, hint: &str) {
    let Some(new) = valid_observation(hint) else {
        return;
    };
    // An event without an observation may acquire one even before its actual
    // occurrence. Only prior observation hints participate in the maximum.
    if metadata_observation(row).is_some_and(|old| old >= new) {
        return;
    }
    set_observation(row, hint);
}

/// Attach an observation hint to a new row. Shared with the sqlx adapter so
/// durable inserts stamp exactly like memory. No-op unless the row is a
/// membership event and the hint is a valid observation.
pub fn set_observation(row: &mut StoredRow, hint: &str) {
    if !is_membership(row.event_type) || valid_observation(hint).is_none() {
        return;
    }
    if !row
        .metadata
        .as_ref()
        .is_some_and(serde_json::Value::is_object)
    {
        row.metadata = Some(serde_json::json!({}));
    }
    row.metadata.as_mut().expect("object metadata")["membershipObservedAt"] = hint.into();
}

/// Rebuild from immutable occurrences plus monotonic observation metadata.
/// Stable row identity breaks equal-occurrence attribution ties; leave wins
/// equal-observation presence ties. Arrival order is never a tie breaker.
/// Shared with the sqlx adapter so the durable read model projects exactly
/// like memory, from the events log rather than arrival-ordered columns.
pub fn project<'a>(rows: impl Iterator<Item = &'a StoredRow>) -> Option<Membership> {
    let mut projection = MembershipProjection::default();
    for row in rows {
        projection.observe(row);
    }
    projection.finish()
}

/// Incremental chronological fold for paged durable reads. Retains only the
/// winning join, presence and inactive timestamp, never the member's history.
#[derive(Default)]
pub struct MembershipProjection {
    join: Option<(String, StoredRow)>,
    presence: Option<(String, StoredRow)>,
    flag: Option<String>,
}

impl MembershipProjection {
    pub fn observe(&mut self, row: &StoredRow) {
        if row.event_type == EventType::MemberJoin {
            if let Some(at) = normalize_timestamp(&row.occurred_at) {
                if self.join.as_ref().is_none_or(|(old, previous)| {
                    at.cmp(old)
                        .then_with(|| previous.idempotency_key.cmp(&row.idempotency_key))
                        .is_ge()
                }) {
                    self.join = Some((at, row.clone()));
                }
            }
        }
        if is_membership(row.event_type) {
            if let Some(at) = observation(row) {
                if self.presence.as_ref().is_none_or(|(old, previous)| {
                    at.cmp(old)
                        .then_with(|| {
                            (row.event_type == EventType::MemberLeave)
                                .cmp(&(previous.event_type == EventType::MemberLeave))
                        })
                        .then_with(|| row.idempotency_key.cmp(&previous.idempotency_key))
                        .is_ge()
                }) {
                    self.presence = Some((at, row.clone()));
                }
            }
        }
        if row.event_type == EventType::MemberInactive {
            if let Some(at) = normalize_timestamp(&row.occurred_at) {
                if self.flag.as_ref().is_none_or(|old| at >= *old) {
                    self.flag = Some(at);
                }
            }
        }
    }

    #[must_use]
    pub fn finish(self) -> Option<Membership> {
        if self.join.is_none() && self.presence.is_none() && self.flag.is_none() {
            return None;
        }
        let inactive_flagged_at = self
            .flag
            .filter(|f| self.join.as_ref().is_none_or(|(at, _)| f > at));
        Some(Membership {
            joined_at: self.join.as_ref().map(|(at, _)| at.clone()),
            join_source: self.join.map(|(_, row)| row.source),
            left_at: self
                .presence
                .filter(|(_, r)| r.event_type == EventType::MemberLeave)
                .and_then(|(_, r)| normalize_timestamp(&r.occurred_at)),
            inactive_flagged_at,
        })
    }
}
