//! Scheduled-events poller (10 m): atomic Discord → `scheduled_events` mirror.
//!
//! Ports `src/jobs/scheduledEvents.ts` from legacy two-bot (frozen `main` @
//! `d5d11793`) as framework-free domain logic: inputs are plain data,
//! outcomes are plain data, and every refusal is unit-testable without Discord
//! or Postgres. Style follows `leveling.rs` / `moderation.rs`.
//!
//! What this module owns:
//! - poll cadence ([`SCHEDULED_EVENTS_INTERVAL_MS`])
//! - status mapping ([`EventStatus`])
//! - response normalization ([`normalize_events`])
//! - skip outcomes ([`ScheduledEventsSkip`])
//! - the validated row shape ([`ScheduledEvent`])
//!
//! Deliberately out of scope: fetching `/guilds/{id}/scheduled-events` (the
//! S4 REST executor slice owns that when it lands), running the timer, and
//! SQL. The sqlx swap lives in [`crate::website_store::replace_events`]; the
//! tick recipe is: acquire [`crate::community_snapshots::JobGate`] → fetch via
//! REST → [`normalize_events`] (`None` = [`ScheduledEventsSkip::InvalidResponse`],
//! fetch failure = `DiscordReadFailed`) → swap via the store → drop the guard.
//! A failed or malformed read leaves the last good snapshot in place rather
//! than publishing "no events" as a transport error — including leaving it at
//! zero rows. A successful empty response *does* delete the last event.

/// Poll cadence (legacy `SCHEDULED_EVENTS_INTERVAL_MS`).
pub const SCHEDULED_EVENTS_INTERVAL_MS: u64 = 10 * 60 * 1000;

/// Discord event status (legacy `STATUS` map over the API integers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventStatus {
    Scheduled,
    Active,
    Completed,
    Cancelled,
}

impl EventStatus {
    /// DB / contract word (legacy `status` column + `web_v1` filter).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Map the Discord API integer (legacy `STATUS`).
    #[must_use]
    pub fn from_api(status: i64) -> Option<Self> {
        match status {
            1 => Some(Self::Scheduled),
            2 => Some(Self::Active),
            3 => Some(Self::Completed),
            4 => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Parse a DB / contract word.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "scheduled" => Some(Self::Scheduled),
            "active" => Some(Self::Active),
            "completed" => Some(Self::Completed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// One raw Discord scheduled-event payload (legacy `RawScheduledEvent`).
/// `None` fields model the legacy `undefined` branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawScheduledEvent {
    pub id: Option<String>,
    pub name: Option<String>,
    pub scheduled_start_time: Option<String>,
    pub channel_id: Option<String>,
    pub description: Option<String>,
    pub status: Option<i64>,
}

/// One validated mirror row (legacy `ScheduledEvent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledEvent {
    pub id: String,
    pub name: String,
    /// UTC millis ISO-8601 (`toISOString` shape, e.g. `2026-09-06T17:30:00.000Z`).
    pub starts_at: String,
    pub channel_id: Option<String>,
    pub description: Option<String>,
    pub status: EventStatus,
}

/// Synchronous per-row mirror seam for internal event actions. A successful
/// action must await this write; the poller's whole-guild replacement is not a
/// substitute because it would erase unrelated rows.
pub trait ScheduledEventMirror: Send + Sync {
    fn upsert(
        &self,
        guild_id: &str,
        observed_at: &str,
        event: &ScheduledEvent,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// Why a poll tick recorded nothing (legacy `ScheduledEventsResult.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduledEventsSkip {
    DiscordReadFailed,
    InvalidResponse,
}

/// Normalize one raw payload (legacy `normalize`): id, name,
/// `scheduled_start_time` (any parseable date, rendered UTC millis) and a
/// known status are all required. `channel_id` / `description` keep only
/// string values, else `None`.
#[must_use]
pub fn normalize_event(raw: &RawScheduledEvent) -> Option<ScheduledEvent> {
    let id = raw.id.as_deref().filter(|s| !s.is_empty())?;
    let name = raw.name.as_deref().filter(|s| !s.is_empty())?;
    let start = raw.scheduled_start_time.as_deref()?;
    let status = raw.status.and_then(EventStatus::from_api)?;
    Some(ScheduledEvent {
        id: id.to_owned(),
        name: name.to_owned(),
        starts_at: normalize_timestamp(start)?,
        channel_id: raw.channel_id.clone(),
        description: raw.description.clone(),
        status,
    })
}

/// Normalize a whole poll response (legacy `raw.map(normalize)` + the
/// `some(null)` rejection): one malformed event rejects the snapshot.
#[must_use]
pub fn normalize_events(raw: &[RawScheduledEvent]) -> Option<Vec<ScheduledEvent>> {
    raw.iter().map(normalize_event).collect()
}

/// Parse the complete RFC 3339 instant fallibly, then reuse the UTC millis
/// renderer (legacy `new Date(s).toISOString()`). Malformed timestamps must
/// reject the whole response, not panic or silently ignore trailing input.
/// Source: https://docs.rs/time/0.3.55/time/format_description/well_known/struct.Rfc3339.html
fn normalize_timestamp(s: &str) -> Option<String> {
    let instant =
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    let millis = i64::try_from(instant.unix_timestamp_nanos().div_euclid(1_000_000)).ok()?;
    Some(crate::funnel::format_iso_millis(millis))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(id: &str, name: &str, start: &str, status: i64) -> RawScheduledEvent {
        RawScheduledEvent {
            id: Some(id.to_owned()),
            name: Some(name.to_owned()),
            scheduled_start_time: Some(start.to_owned()),
            channel_id: Some("voice-1".to_owned()),
            description: Some("Join the weekly games night.".to_owned()),
            status: Some(status),
        }
    }

    #[test]
    fn interval_is_the_contract_value() {
        assert_eq!(SCHEDULED_EVENTS_INTERVAL_MS, 10 * 60 * 1000);
    }

    #[test]
    fn status_maps_the_four_api_integers() {
        assert_eq!(EventStatus::from_api(1), Some(EventStatus::Scheduled));
        assert_eq!(EventStatus::from_api(2), Some(EventStatus::Active));
        assert_eq!(EventStatus::from_api(3), Some(EventStatus::Completed));
        assert_eq!(EventStatus::from_api(4), Some(EventStatus::Cancelled));
        assert_eq!(EventStatus::from_api(0), None);
        assert_eq!(EventStatus::from_api(5), None);
        assert_eq!(
            EventStatus::parse("scheduled"),
            Some(EventStatus::Scheduled)
        );
        assert_eq!(EventStatus::parse("active"), Some(EventStatus::Active));
        assert_eq!(EventStatus::parse("bogus"), None);
    }

    #[test]
    fn normalizes_offsets_to_utc_millis() {
        // Legacy test vector: +01:00 renders back one hour earlier in Z.
        let event = normalize_event(&raw(
            "event-1",
            "Sunday Squad",
            "2026-09-06T18:30:00+01:00",
            1,
        ))
        .expect("normalizes");
        assert_eq!(event.starts_at, "2026-09-06T17:30:00.000Z");
        assert_eq!(event.status, EventStatus::Scheduled);
        assert_eq!(event.channel_id.as_deref(), Some("voice-1"));
    }

    #[test]
    fn fractions_and_pre_epoch_instants_keep_millisecond_precision() {
        for (start, expected) in [
            ("2026-09-06T18:00:00.123456Z", "2026-09-06T18:00:00.123Z"),
            ("2026-09-06T18:00:00.1-01:30", "2026-09-06T19:30:00.100Z"),
            ("1969-12-31T23:59:59.999999Z", "1969-12-31T23:59:59.999Z"),
        ] {
            let event = normalize_event(&raw("x", "Good event", start, 1)).expect("normalizes");
            assert_eq!(event.starts_at, expected, "{start}");
        }
    }

    #[test]
    fn rejects_malformed_timestamp_shapes_and_dates() {
        for start in [
            "2026-09-06T18:00:00.123.extraZ",
            "2026-09-06T18:00:00+0é0",
            "2026-09-06T18:00:00.123Zextra",
            "2026-09-06T18:00:00.Z",
            "2026-09-06T18:00:00+24:00",
            "2026-09-06T18:00:00+01:60",
            "2026-02-30T18:00:00Z",
            "9223372036854775807-09-06T18:00:00Z",
            "é026-09-06T18:00:00Z",
            "2026-09-06T18:00:00",
        ] {
            assert!(
                normalize_event(&raw("x", "Bad event", start, 1)).is_none(),
                "{start}"
            );
        }
    }

    #[test]
    fn malformed_offset_rejects_the_whole_snapshot_without_panicking() {
        let response = [
            raw("good", "Good event", "2026-09-06T18:00:00.000Z", 1),
            raw("bad", "Malformed offset", "2026-09-06T18:00:00+0é0", 1),
        ];
        assert!(normalize_events(&response).is_none());
    }

    #[test]
    fn trailing_fraction_garbage_rejects_the_whole_snapshot() {
        let response = [
            raw("good", "Good event", "2026-09-06T18:00:00.000Z", 1),
            raw(
                "bad",
                "Malformed fraction",
                "2026-09-06T18:00:00.123.extraZ",
                1,
            ),
        ];
        assert!(normalize_events(&response).is_none());
    }

    #[test]
    fn non_string_channel_and_description_become_null() {
        let event = normalize_event(&RawScheduledEvent {
            id: Some("event-1".to_owned()),
            name: Some("Good event".to_owned()),
            scheduled_start_time: Some("2026-09-06T18:00:00.000Z".to_owned()),
            channel_id: None,
            description: None,
            status: Some(1),
        })
        .expect("normalizes");
        assert_eq!(event.channel_id, None);
        assert_eq!(event.description, None);
    }

    #[test]
    fn rejects_missing_id_name_start_or_status() {
        for broken in [
            RawScheduledEvent {
                id: None,
                ..raw("x", "n", "2026-09-06T18:00:00.000Z", 1)
            },
            RawScheduledEvent {
                id: Some(String::new()),
                ..raw("x", "n", "2026-09-06T18:00:00.000Z", 1)
            },
            RawScheduledEvent {
                name: None,
                ..raw("x", "n", "2026-09-06T18:00:00.000Z", 1)
            },
            RawScheduledEvent {
                scheduled_start_time: Some("not-a-date".to_owned()),
                ..raw("x", "n", "2026-09-06T18:00:00.000Z", 1)
            },
            RawScheduledEvent {
                status: Some(99),
                ..raw("x", "n", "2026-09-06T18:00:00.000Z", 1)
            },
            RawScheduledEvent {
                status: None,
                ..raw("x", "n", "2026-09-06T18:00:00.000Z", 1)
            },
        ] {
            assert!(normalize_event(&broken).is_none(), "{broken:?}");
        }
    }

    #[test]
    fn one_malformed_event_rejects_the_whole_snapshot() {
        assert!(normalize_events(&[
            raw("event-1", "Good event", "2026-09-06T18:00:00.000Z", 1),
            RawScheduledEvent {
                scheduled_start_time: None,
                ..raw("event-2", "No start time", "2026-09-06T18:00:00.000Z", 1)
            },
        ])
        .is_none());
        assert_eq!(
            normalize_events(&[]).expect("empty is valid").len(),
            0,
            "a successful empty response is a valid empty mirror"
        );
    }
}
