//! RSVP + host check-in attendance domain: transitions, totals, reply text.
//!
//! Slice TOG-10083 (S4 RSVP). Ports the DB-free, framework-free heart of
//! legacy two-bot RSVP (`src/announcements/service.ts`: `rsvp`, `attendance`;
//! `src/announcements/discord.ts`: the `/rsvp` and `/attendance` replies) and
//! host check-in (`src/analytics/communityAttendance.ts`:
//! `recordCommunityAttendance`; `src/analytics/communityFacts.ts`:
//! `recordAttendance`) as pure functions over plain data. Storage lands in
//! the `rsvp_store` module (sqlx, behind the `db` feature); the interaction
//! router (TOG-10075) and REST executor (TOG-10076) consume the outcome enums
//! and reply-text helpers here, so every transition and refusal is
//! unit-testable without Discord or Postgres.
//!
//! Source files (legacy `two-bot`, frozen `main`):
//! - transitions + totals: `src/announcements/service.ts` (`rsvp`,
//!   `attendance`) + `src/announcements/store.ts` (`putRsvp`, `listRsvps`).
//! - replies: `src/announcements/discord.ts` (`handleCommand` `rsvp` /
//!   `attendance` cases) and `src/analytics/communityAttendance.ts` (check-in
//!   replies).
//! - check-in fact: `src/analytics/communityFacts.ts` (`recordAttendance`:
//!   the `rsvp` proof writes nothing, every other proof appends one
//!   `event_attended` fact keyed `event-attended:{occurrence}:{actor}`).
//!
//! Naming: the scorecard `/attendance` keeps the `attendance` name; the RSVP
//! totals command is namespaced to `rsvp-attendance` (parity §1 #24–#25, see
//! `feature_commands.rs`). The merge in `commands.rs` is first-wins, so a
//! repeated name can never silently drop a command again — the registry test
//! below pins the published set.
//!
//! Deliberately out of scope: LFG + feeds (TOG-10084/TOG-10085), the full
//! community classifier (S5; check-in carries the minimal bot/human rule
//! until it lands), and Discord delivery (router/executor slices).

use super::commands::{OCCURRENCE_ID_MAX_CHARS, PERM_MANAGE_EVENTS};

/// An RSVP response (legacy `RsvpStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RsvpStatus {
    Going,
    Interested,
    Declined,
}

impl RsvpStatus {
    /// All three responses in legacy choice order.
    pub const ALL: [Self; 3] = [Self::Going, Self::Interested, Self::Declined];

    /// Wire value (`going`, … — legacy `status` column + `/rsvp` choices).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Going => "going",
            Self::Interested => "interested",
            Self::Declined => "declined",
        }
    }

    /// Parse a wire value. Discord enforces the choices, so anything else is
    /// a caller bug, surfaced as [`RsvpError::UnknownStatus`].
    pub fn parse(value: &str) -> Result<Self, RsvpError> {
        match value {
            "going" => Ok(Self::Going),
            "interested" => Ok(Self::Interested),
            "declined" => Ok(Self::Declined),
            other => Err(RsvpError::UnknownStatus(other.to_owned())),
        }
    }
}

impl std::fmt::Display for RsvpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Invalid RSVP input. Messages mirror the legacy throws so router surfacing
/// stays byte-identical.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RsvpError {
    #[error("event id must be a Discord id.")]
    InvalidEventId,
    #[error("unknown RSVP status {0:?}: expected going, interested or declined.")]
    UnknownStatus(String),
}

/// True when `s` is a Discord snowflake (legacy `/^\d{17,20}$/`).
#[must_use]
pub fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// Validate an `/rsvp` or `/rsvp-attendance` event id (legacy
/// `assertSnowflake(eventId, 'event id')`).
pub fn validate_event_id(value: &str) -> Result<String, RsvpError> {
    if is_snowflake(value) {
        Ok(value.to_owned())
    } else {
        Err(RsvpError::InvalidEventId)
    }
}

/// One RSVP row (legacy `EventRsvpRow`): the durable per-member response.
/// `responded_at` is ISO-8601 UTC; the store binds it as `timestamptz`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RsvpRecord {
    pub guild_id: String,
    pub event_id: String,
    pub user_id: String,
    pub status: RsvpStatus,
    pub responded_at: String,
}

/// What one write changed (legacy `putRsvp` is a blind upsert; the read-back
/// of the previous row is what makes going/interested/declined transitions
/// observable to the router).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsvpTransition {
    pub previous: Option<RsvpStatus>,
    pub current: RsvpStatus,
}

impl RsvpTransition {
    /// First response from this member for this event.
    #[must_use]
    pub fn is_new(self) -> bool {
        self.previous.is_none()
    }

    /// The member moved between responses (including re-selecting the same
    /// one — legacy still rewrites `responded_at` and audits every response).
    #[must_use]
    pub fn changed(self) -> bool {
        self.previous.is_some_and(|p| p != self.current)
    }
}

/// Partitioned totals (legacy `service.attendance` return): member ids per
/// response, in store order (`responded_at, user_id`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RsvpTotals {
    pub going: Vec<String>,
    pub interested: Vec<String>,
    pub declined: Vec<String>,
}

impl RsvpTotals {
    /// `(going, interested, declined)` counts for the totals reply.
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize) {
        (self.going.len(), self.interested.len(), self.declined.len())
    }

    /// Total responses across all three buckets.
    #[must_use]
    pub fn total(&self) -> usize {
        self.going.len() + self.interested.len() + self.declined.len()
    }
}

/// Partition rows into totals, preserving input order (legacy filters
/// `going` / `interested` / `declined` over `listRsvps` order).
#[must_use]
pub fn partition_rsvps(records: &[RsvpRecord]) -> RsvpTotals {
    let mut totals = RsvpTotals::default();
    for record in records {
        match record.status {
            RsvpStatus::Going => totals.going.push(record.user_id.clone()),
            RsvpStatus::Interested => totals.interested.push(record.user_id.clone()),
            RsvpStatus::Declined => totals.declined.push(record.user_id.clone()),
        }
    }
    totals
}

/// `/rsvp` reply (legacy `` `RSVP saved: ${status}.` ``, ephemeral).
#[must_use]
pub fn rsvp_saved_text(status: RsvpStatus) -> String {
    format!("RSVP saved: {}.", status.as_str())
}

/// `/rsvp-attendance` totals reply (legacy `` `Going: ${g}\nInterested:
/// ${i}\nDeclined: ${d}` ``, ephemeral).
#[must_use]
pub fn attendance_totals_text(totals: &RsvpTotals) -> String {
    let (going, interested, declined) = totals.counts();
    format!("Going: {going}\nInterested: {interested}\nDeclined: {declined}")
}

/// Host check-in refusal or malformed input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckinError {
    /// Legacy LFG wording, reused: the scorecard `/attendance` command gates
    /// `ManageEvents` in Discord, and the router enforces it server-side too.
    #[error("Manage Events permission is required.")]
    MissingManageEvents,
    #[error("event occurrence must not be empty.")]
    EmptyOccurrence,
    // Literal bound (matches the `ReasonError::TooLong` precedent): the
    // boundary test below pins it against `OCCURRENCE_ID_MAX_CHARS`.
    #[error("\"event occurrence\" is longer than 128 characters")]
    OccurrenceTooLong,
    /// Free text with no live-event anchor: no Discord or guild authority can
    /// vouch for the occurrence, so the handler must refuse before any write.
    /// The format itself stays accepted (parity §1 #12) — only unanchored
    /// values refuse. Never echoes the input (overlong-adjacent hygiene).
    #[error("\"event occurrence\" must be a scheduled event id, optionally with a :label suffix")]
    UnanchoredOccurrence,
}

/// Server-side `ManageEvents` gate for host check-in (parity §1 #12: the
/// scorecard `/attendance` handler). Discord enforces the command's
/// `default_member_permissions`; this is the defense-in-depth check the
/// router runs before recording.
pub fn require_manage_events(permissions: u64) -> Result<(), CheckinError> {
    if permissions & PERM_MANAGE_EVENTS == PERM_MANAGE_EVENTS {
        Ok(())
    } else {
        Err(CheckinError::MissingManageEvents)
    }
}

/// Validate a host check-in occurrence id (legacy trims the
/// `event-occurrence` option; an empty id records nothing addressable, so it
/// is refused instead). Overlong ids are refused before any record operation:
/// the recorded/duplicate replies interpolate the whole id, so an unbounded
/// id could make the acknowledgement unsendable after recording. The length
/// counts UTF-16 units (legacy JS `length` semantics — astral counts 2) and
/// matches the `max_length` the command shape advertises.
pub fn validate_occurrence_id(value: &str) -> Result<String, CheckinError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(CheckinError::EmptyOccurrence)
    } else if trimmed.encode_utf16().count() > OCCURRENCE_ID_MAX_CHARS {
        Err(CheckinError::OccurrenceTooLong)
    } else {
        Ok(trimmed.to_owned())
    }
}

/// A validated attendance occurrence plus the live scheduled-event anchor that
/// proves it belongs to the interaction's guild (RA-02). The handler looks the
/// anchor up with the same live-event read the `/rsvp` path uses; only that
/// lookup passing makes the canonical id recordable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttendanceOccurrence {
    /// Live scheduled-event id the occurrence is anchored to (Discord is the
    /// authority; the handler verifies id, guild and status before writing).
    pub anchor_event_id: String,
    /// Recorded `event_occurrence_id`: the event id itself, or
    /// `{anchor}:{label}` for a labeled repeat occurrence. Never longer than
    /// the validated input it derives from, so the length bound still holds.
    pub canonical_id: String,
}

/// Resolve a validated occurrence string into its trusted form (RA-02).
///
/// A bare Discord snowflake names the scheduled event itself. Any other text
/// must anchor to one as `{event_id}:{label}` so the handler can prove the
/// event exists in this guild before recording; the free-text format stays
/// accepted (parity §1 #12) while bare slugs with no anchor refuse, because no
/// live authority could vouch for them. Input must already be trimmed
/// ([`validate_occurrence_id`]); the anchor and label trim again so the
/// canonical id has one stable shape.
pub fn parse_attendance_occurrence(value: &str) -> Result<AttendanceOccurrence, CheckinError> {
    if is_snowflake(value) {
        return Ok(AttendanceOccurrence {
            anchor_event_id: value.to_owned(),
            canonical_id: value.to_owned(),
        });
    }
    let (anchor_raw, label_raw) = value
        .split_once(':')
        .ok_or(CheckinError::UnanchoredOccurrence)?;
    let anchor = anchor_raw.trim();
    let label = label_raw.trim();
    if !is_snowflake(anchor) || label.is_empty() {
        return Err(CheckinError::UnanchoredOccurrence);
    }
    Ok(AttendanceOccurrence {
        anchor_event_id: anchor.to_owned(),
        canonical_id: format!("{anchor}:{label}"),
    })
}

/// Attendance proof (legacy `AttendanceProof`). Only `host_checkin` is
/// produced by this slice; the other variants are carried so the
/// `rsvp`-writes-nothing rule ports exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttendanceProof {
    HostCheckin,
    DurableCheckin,
    Voice600s,
    Rsvp,
}

impl AttendanceProof {
    /// Metadata value (legacy `proof` field).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HostCheckin => "host_checkin",
            Self::DurableCheckin => "durable_checkin",
            Self::Voice600s => "voice_600s",
            Self::Rsvp => "rsvp",
        }
    }

    /// Whether this proof appends a fact (legacy `recordAttendance` returns
    /// `false` without writing for the `rsvp` proof).
    #[must_use]
    pub fn writes_fact(self) -> bool {
        !matches!(self, Self::Rsvp)
    }
}

impl std::fmt::Display for AttendanceProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Verified-attendance fact identity (legacy `recordAttendance`):
/// `event_type = 'event_attended'`, `source_event_id =
/// '{occurrence}:{actor}'`, `source = 'event:{occurrence}'`, idempotency key
/// `event-attended:{occurrence}:{actor}`.
pub const ATTENDANCE_EVENT_TYPE: &str = "event_attended";

/// `source_event_id` for a check-in fact.
#[must_use]
pub fn checkin_source_event_id(event_occurrence_id: &str, member_id: &str) -> String {
    format!("{event_occurrence_id}:{member_id}")
}

/// `source` for a check-in fact.
#[must_use]
pub fn checkin_source(event_occurrence_id: &str) -> String {
    format!("event:{event_occurrence_id}")
}

/// Idempotency key for a check-in fact: retries dedupe, distinct members and
/// occurrences never collide.
#[must_use]
pub fn checkin_idempotency_key(event_occurrence_id: &str, member_id: &str) -> String {
    format!("event-attended:{event_occurrence_id}:{member_id}")
}

/// `metadata` JSON for a check-in fact (legacy `{ eventOccurrenceId, proof }`).
#[must_use]
pub fn checkin_metadata_json(event_occurrence_id: &str, proof: AttendanceProof) -> String {
    serde_json::json!({
        "eventOccurrenceId": event_occurrence_id,
        "proof": proof.as_str(),
    })
    .to_string()
}

/// Minimal check-in classification (legacy `CommunityClassifier` precedence
/// reduced to the bot flag: bots classify `bot`, everyone else
/// `eligible_human`). The full classifier — staff/raid/staging/test actor
/// lists — is S5-owned; the store takes explicit classification inputs so it
/// picks the S5 classifier up without a signature change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttendanceClassification {
    pub classification: &'static str,
    pub matched_rule: &'static str,
}

/// Classify a host check-in subject (legacy rules `discord_bot` /
/// `no_exclusion_matched`). Bots are recorded — never refused — so the fact
/// retains the `bot` classification and can never enter human attendance.
#[must_use]
pub fn checkin_classification(member_is_bot: bool) -> AttendanceClassification {
    if member_is_bot {
        AttendanceClassification {
            classification: "bot",
            matched_rule: "discord_bot",
        }
    } else {
        AttendanceClassification {
            classification: "eligible_human",
            matched_rule: "no_exclusion_matched",
        }
    }
}

/// Host check-in recorded reply (legacy `` `Recorded <@${id}> for event
/// occurrence \`${occ}\`.` ``).
#[must_use]
pub fn checkin_recorded_text(member_id: &str, event_occurrence_id: &str) -> String {
    format!("Recorded <@{member_id}> for event occurrence `{event_occurrence_id}`.")
}

/// Host check-in duplicate reply (legacy `` `Attendance for <@${id}> and
/// event occurrence \`${occ}\` was already recorded.` ``).
#[must_use]
pub fn checkin_duplicate_text(member_id: &str, event_occurrence_id: &str) -> String {
    format!(
        "Attendance for <@{member_id}> and event occurrence \
         `{event_occurrence_id}` was already recorded."
    )
}

/// Audit action for RSVP writes (legacy `event.rsvp`; every response is
/// audited, including repeats).
pub const RSVP_AUDIT_ACTION: &str = "event.rsvp";

/// RA-03 admission bounds (TOG-19773): bursts cannot grow rows without bound.
///
/// * `MAX_RSVPS_PER_EVENT` caps distinct RSVP rows per scheduled event. One
///   member still holds exactly one row (the `event_rsvps` primary key);
///   repeat responses rewrite that row.
/// * `MAX_CHECKINS_PER_OCCURRENCE` caps attendance facts per occurrence. One
///   member still holds exactly one fact (the idempotency key on
///   `community_facts`).
/// * `MAX_RSVP_WRITES_PER_USER_PER_MINUTE` caps RSVP writes per member per
///   guild per UTC minute, ledgered in `announcements_audit_log`. Genuine
///   double-taps pass; spam bursts are refused before any row write.
///
/// The store enforces all three inside its write transactions (race-safe via
/// the advisory locks there); the discord layer maps the refusals to replies.
/// 1,000 keeps per-event totals and audit volume bounded while staying far
/// above real event sizes on a single-shard bot; 20/minute absorbs client
/// retries while capping one member's audit churn.
pub const MAX_RSVPS_PER_EVENT: i64 = 1_000;
pub const MAX_CHECKINS_PER_OCCURRENCE: i64 = 1_000;
pub const MAX_RSVP_WRITES_PER_USER_PER_MINUTE: i64 = 20;

/// RA-03 retention floors: history purges must never delete rows newer than
/// these horizons, so live recovery state and the audit trail survive.
/// RSVP rows back live events and recent totals; audit rows are the security
/// audit trail, so the audit floor is the stricter of the two and governs
/// joint purges (see `rsvp_store::prune_rsvp_history`).
pub const RSVP_RETENTION_DAYS: i64 = 90;
pub const AUDIT_RETENTION_DAYS: i64 = 365;

/// RA-03 refusal replies (new behavior, no legacy text to match):
/// capacity/rate refusals name the bound without echoing caller input.
#[must_use]
pub fn rsvp_event_full_text() -> String {
    format!("This event has reached its RSVP limit ({MAX_RSVPS_PER_EVENT} responses).")
}

/// RA-03 refusal reply for a per-user RSVP burst.
#[must_use]
pub fn rsvp_rate_limited_text() -> String {
    "You are responding too quickly. Wait a minute, then try again.".to_owned()
}

/// RA-03 refusal reply for a full occurrence.
#[must_use]
pub fn checkin_occurrence_full_text() -> String {
    format!(
        "This occurrence has reached its check-in limit ({MAX_CHECKINS_PER_OCCURRENCE} check-ins)."
    )
}

/// One `announcements_audit_log` row (legacy `AnnouncementsAuditInput` plus
/// the caller-supplied id and timestamp). The id is an input — not generated
/// here — so this module stays free of randomness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RsvpAudit {
    pub id: String,
    pub guild_id: String,
    pub actor_id: Option<String>,
    pub action: String,
    pub target_key: Option<String>,
    pub outcome: String,
    pub reason: Option<String>,
    pub created_at: String,
}

impl RsvpAudit {
    /// Audit row for one RSVP response: action `event.rsvp`, target the
    /// event, outcome the new status (legacy `service.rsvp` audit call).
    #[must_use]
    pub fn for_rsvp(id: &str, record: &RsvpRecord) -> Self {
        Self {
            id: id.to_owned(),
            guild_id: record.guild_id.clone(),
            actor_id: Some(record.user_id.clone()),
            action: RSVP_AUDIT_ACTION.to_owned(),
            target_key: Some(record.event_id.clone()),
            outcome: record.status.as_str().to_owned(),
            reason: None,
            created_at: record.responded_at.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn property_rsvp_parsers_match_exact_wire_contract(
            text in proptest::collection::vec(any::<char>(), 0..128)
                .prop_map(|chars| chars.into_iter().collect::<String>()),
        ) {
            let expected_id = (17..=20).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit());
            prop_assert_eq!(validate_event_id(&text).is_ok(), expected_id);
            if let Ok(id) = validate_event_id(&text) {
                prop_assert_eq!(validate_event_id(&id), Ok(id));
            }
            let expected_status = ["going", "interested", "declined"].contains(&text.as_str());
            prop_assert_eq!(RsvpStatus::parse(&text).is_ok(), expected_status);
        }

        #[test]
        fn property_rsvp_values_round_trip(
            digits in "[0-9]{0,23}",
            status in prop::sample::select(RsvpStatus::ALL.to_vec()),
        ) {
            prop_assert_eq!(validate_event_id(&digits).is_ok(), (17..=20).contains(&digits.len()));
            if (17..=20).contains(&digits.len()) {
                prop_assert_eq!(validate_event_id(&digits), Ok(digits));
            }
            prop_assert_eq!(RsvpStatus::parse(&status.to_string()), Ok(status));
        }
    }
    use crate::commands::merge_commands;
    use crate::feature_commands::feature_commands;
    use crate::moderation::moderation_commands;

    const GUILD: &str = "1545644954272137297";
    const EVENT: &str = "1546451670500642999";
    const USER: &str = "1546451670500642888";

    fn record(status: RsvpStatus, user: &str, at: &str) -> RsvpRecord {
        RsvpRecord {
            guild_id: GUILD.to_owned(),
            event_id: EVENT.to_owned(),
            user_id: user.to_owned(),
            status,
            responded_at: at.to_owned(),
        }
    }

    #[test]
    fn status_round_trips_wire_values_in_legacy_order() {
        let values: Vec<_> = RsvpStatus::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(values, ["going", "interested", "declined"]);
        for status in RsvpStatus::ALL {
            assert_eq!(RsvpStatus::parse(status.as_str()), Ok(status));
            assert_eq!(status.to_string(), status.as_str());
        }
        assert!(matches!(
            RsvpStatus::parse("Going"),
            Err(RsvpError::UnknownStatus(_))
        ));
        assert!(matches!(
            RsvpStatus::parse(""),
            Err(RsvpError::UnknownStatus(_))
        ));
    }

    #[test]
    fn event_id_must_be_a_snowflake() {
        assert_eq!(validate_event_id(EVENT), Ok(EVENT.to_owned()));
        assert_eq!(validate_event_id("1"), Err(RsvpError::InvalidEventId));
        assert_eq!(
            validate_event_id("not-an-id"),
            Err(RsvpError::InvalidEventId)
        );
        assert_eq!(
            validate_event_id(&"9".repeat(21)),
            Err(RsvpError::InvalidEventId)
        );
        // 17-digit lower bound and 20-digit upper bound both pass.
        assert!(validate_event_id(&"1".repeat(17)).is_ok());
        assert!(validate_event_id(&"9".repeat(20)).is_ok());
        assert!(!is_snowflake("1234567890123456x"));
    }

    #[test]
    fn transitions_distinguish_new_change_and_repeat() {
        let fresh = RsvpTransition {
            previous: None,
            current: RsvpStatus::Going,
        };
        assert!(fresh.is_new());
        assert!(!fresh.changed());
        let moved = RsvpTransition {
            previous: Some(RsvpStatus::Going),
            current: RsvpStatus::Interested,
        };
        assert!(!moved.is_new());
        assert!(moved.changed());
        // Re-selecting the same status still rewrites responded_at + audits.
        let repeat = RsvpTransition {
            previous: Some(RsvpStatus::Going),
            current: RsvpStatus::Going,
        };
        assert!(!repeat.changed());
    }

    #[test]
    fn totals_partition_preserves_store_order() {
        let rows = vec![
            record(RsvpStatus::Going, USER, "2026-09-10T10:00:00.000Z"),
            record(
                RsvpStatus::Declined,
                "1546451670500642777",
                "2026-09-10T10:01:00.000Z",
            ),
            record(
                RsvpStatus::Going,
                "1546451670500642778",
                "2026-09-10T10:02:00.000Z",
            ),
            record(
                RsvpStatus::Interested,
                "1546451670500642779",
                "2026-09-10T10:03:00.000Z",
            ),
        ];
        let totals = partition_rsvps(&rows);
        assert_eq!(totals.going, [USER, "1546451670500642778"]);
        assert_eq!(totals.interested, ["1546451670500642779"]);
        assert_eq!(totals.declined, ["1546451670500642777"]);
        assert_eq!(totals.counts(), (2, 1, 1));
        assert_eq!(totals.total(), 4);
        assert_eq!(partition_rsvps(&[]), RsvpTotals::default());
    }

    #[test]
    fn reply_texts_match_legacy_byte_for_byte() {
        assert_eq!(
            rsvp_saved_text(RsvpStatus::Interested),
            "RSVP saved: interested."
        );
        assert_eq!(
            attendance_totals_text(&RsvpTotals {
                going: vec!["a".to_owned(), "b".to_owned()],
                interested: vec!["c".to_owned()],
                declined: vec![],
            }),
            "Going: 2\nInterested: 1\nDeclined: 0"
        );
        assert_eq!(
            checkin_recorded_text("human-1", "event-1"),
            "Recorded <@human-1> for event occurrence `event-1`."
        );
        assert_eq!(
            checkin_duplicate_text("human-1", "event-1"),
            "Attendance for <@human-1> and event occurrence `event-1` was already recorded."
        );
    }

    #[test]
    fn checkin_gate_requires_manage_events() {
        assert!(require_manage_events(PERM_MANAGE_EVENTS).is_ok());
        // Combined with unrelated bits the gate still passes.
        assert!(require_manage_events(PERM_MANAGE_EVENTS | 0x20).is_ok());
        assert_eq!(
            require_manage_events(0),
            Err(CheckinError::MissingManageEvents)
        );
        // Moderation bits alone do not open check-in.
        assert_eq!(
            require_manage_events(4),
            Err(CheckinError::MissingManageEvents)
        );
    }

    #[test]
    fn occurrence_id_trims_and_rejects_empty() {
        assert_eq!(
            validate_occurrence_id("  event-1  "),
            Ok("event-1".to_owned())
        );
        assert_eq!(
            validate_occurrence_id("   "),
            Err(CheckinError::EmptyOccurrence)
        );
        assert_eq!(
            validate_occurrence_id(""),
            Err(CheckinError::EmptyOccurrence)
        );
    }

    #[test]
    fn attendance_occurrence_binds_every_format_to_a_live_event_anchor() {
        // Bare snowflake: the event itself is the occurrence.
        assert_eq!(
            parse_attendance_occurrence(EVENT),
            Ok(AttendanceOccurrence {
                anchor_event_id: EVENT.to_owned(),
                canonical_id: EVENT.to_owned(),
            })
        );
        // Anchored free text keeps the full label as the recorded id.
        assert_eq!(
            parse_attendance_occurrence(&format!("{EVENT}:2026-09-30")),
            Ok(AttendanceOccurrence {
                anchor_event_id: EVENT.to_owned(),
                canonical_id: format!("{EVENT}:2026-09-30"),
            })
        );
        // Anchor and label trim so one occurrence has one stable id.
        assert_eq!(
            parse_attendance_occurrence(&format!("  {EVENT} : 2026-09-30  ")),
            Ok(AttendanceOccurrence {
                anchor_event_id: EVENT.to_owned(),
                canonical_id: format!("{EVENT}:2026-09-30"),
            })
        );
        // Labels may contain further colons; the first one separates.
        assert_eq!(
            parse_attendance_occurrence(&format!("{EVENT}:week:3")),
            Ok(AttendanceOccurrence {
                anchor_event_id: EVENT.to_owned(),
                canonical_id: format!("{EVENT}:week:3"),
            })
        );
        // Bare slugs name no anchorable event, so no live authority could vouch
        // for them: refused without removing the free-text format.
        let overlong_snowflake = "9".repeat(21);
        for bare in [
            "weekly-standup-2026-10-03",
            "weekly:2026-09-30",
            "event-1",
            "not-an-id",
            "1",
            overlong_snowflake.as_str(),
        ] {
            assert_eq!(
                parse_attendance_occurrence(bare),
                Err(CheckinError::UnanchoredOccurrence),
                "{bare} names no anchor"
            );
        }
        // Empty anchor or empty label refuses.
        for malformed in [
            ":label".to_owned(),
            format!("{EVENT}:"),
            format!("{EVENT}:   "),
            " : ".to_owned(),
            ":".to_owned(),
        ] {
            assert_eq!(
                parse_attendance_occurrence(&malformed),
                Err(CheckinError::UnanchoredOccurrence),
                "{malformed} is not anchored"
            );
        }
        // The refusal names the accepted shape without echoing the input.
        let err = parse_attendance_occurrence("weekly-standup-2026-10-03")
            .expect_err("bare slug refuses");
        let text = err.to_string();
        assert!(
            !text.contains("weekly-standup-2026-10-03"),
            "refusal echoes nothing: {text}"
        );
        assert!(text.contains(":label"), "refusal guides hosts: {text}");
        // Canonical ids never exceed the validated input length, so the
        // advertised bound still governs the recorded reply.
        let canonical = parse_attendance_occurrence(&format!("  {EVENT} : x  "))
            .expect("parses")
            .canonical_id;
        assert!(canonical.encode_utf16().count() <= OCCURRENCE_ID_MAX_CHARS);
    }

    #[test]
    fn occurrence_id_refuses_overlong_ids_before_recording() {
        use crate::message_safety::CONTENT_LIMIT;
        // Inclusive edge accepts; the adjacent outsider refuses.
        let at_bound = "x".repeat(OCCURRENCE_ID_MAX_CHARS);
        assert_eq!(validate_occurrence_id(&at_bound), Ok(at_bound.clone()));
        assert_eq!(
            validate_occurrence_id(&"x".repeat(OCCURRENCE_ID_MAX_CHARS + 1)),
            Err(CheckinError::OccurrenceTooLong)
        );
        // Trimmed length governs: padding around an at-bound id accepts.
        assert_eq!(
            validate_occurrence_id(&format!("  {at_bound}  ")),
            Ok(at_bound.clone())
        );
        // Whitespace that trims to empty refuses as empty, not too-long.
        assert_eq!(
            validate_occurrence_id(&" ".repeat(OCCURRENCE_ID_MAX_CHARS + 10)),
            Err(CheckinError::EmptyOccurrence)
        );
        // Error text names the bound without echoing the oversized id.
        let oversized = "y".repeat(OCCURRENCE_ID_MAX_CHARS + 1);
        let err = validate_occurrence_id(&oversized).expect_err("overlong refuses");
        let text = err.to_string();
        assert!(!text.contains(&oversized));
        assert!(
            text.contains(&OCCURRENCE_ID_MAX_CHARS.to_string()),
            "literal error bound tracks the shared const"
        );
        // Astral boundary (legacy JS `length` counts UTF-16 units): 64 emoji
        // are 128 units and accept; 65 emoji are 130 units and refuse.
        let astral_at = "\u{1F600}".repeat(64);
        assert_eq!(astral_at.encode_utf16().count(), OCCURRENCE_ID_MAX_CHARS);
        assert_eq!(validate_occurrence_id(&astral_at), Ok(astral_at.clone()));
        assert_eq!(
            validate_occurrence_id(&"\u{1F600}".repeat(65)),
            Err(CheckinError::OccurrenceTooLong)
        );
        // Accepted recorded/duplicate replies fit the Discord content limit,
        // even with the longest member id and a bound-sized occurrence.
        for member in ["1".to_owned(), "9".repeat(20)] {
            for reply in [
                checkin_recorded_text(&member, &at_bound),
                checkin_duplicate_text(&member, &at_bound),
                checkin_recorded_text(&member, &astral_at),
                checkin_duplicate_text(&member, &astral_at),
            ] {
                assert!(
                    reply.encode_utf16().count() <= CONTENT_LIMIT,
                    "reply fits: {}",
                    reply.encode_utf16().count()
                );
            }
        }
    }

    #[test]
    fn rsvp_proof_writes_no_fact() {
        assert!(AttendanceProof::HostCheckin.writes_fact());
        assert!(AttendanceProof::DurableCheckin.writes_fact());
        assert!(AttendanceProof::Voice600s.writes_fact());
        assert!(!AttendanceProof::Rsvp.writes_fact());
        assert_eq!(AttendanceProof::HostCheckin.as_str(), "host_checkin");
        assert_eq!(AttendanceProof::Rsvp.to_string(), "rsvp");
    }

    #[test]
    fn checkin_fact_identity_matches_legacy() {
        assert_eq!(ATTENDANCE_EVENT_TYPE, "event_attended");
        assert_eq!(
            checkin_source_event_id("event-1", "human-1"),
            "event-1:human-1"
        );
        assert_eq!(checkin_source("event-1"), "event:event-1");
        assert_eq!(
            checkin_idempotency_key("event-1", "human-1"),
            "event-attended:event-1:human-1"
        );
        assert_eq!(
            checkin_metadata_json("event-1", AttendanceProof::HostCheckin),
            r#"{"eventOccurrenceId":"event-1","proof":"host_checkin"}"#
        );
    }

    #[test]
    fn checkin_classification_keeps_bots_out_of_human_attendance() {
        let human = checkin_classification(false);
        assert_eq!(human.classification, "eligible_human");
        assert_eq!(human.matched_rule, "no_exclusion_matched");
        // Bots are recorded, never refused, with the bot classification.
        let bot = checkin_classification(true);
        assert_eq!(bot.classification, "bot");
        assert_eq!(bot.matched_rule, "discord_bot");
    }

    #[test]
    fn audit_row_carries_event_and_outcome() {
        let audit = RsvpAudit::for_rsvp(
            "audit-id",
            &record(RsvpStatus::Declined, USER, "2026-09-10T10:00:00.000Z"),
        );
        assert_eq!(audit.action, "event.rsvp");
        assert_eq!(audit.actor_id.as_deref(), Some(USER));
        assert_eq!(audit.target_key.as_deref(), Some(EVENT));
        assert_eq!(audit.outcome, "declined");
        assert_eq!(audit.reason, None);
        assert_eq!(audit.created_at, "2026-09-10T10:00:00.000Z");
    }

    #[test]
    fn admission_bounds_and_retention_floors_are_pinned() {
        // RA-03: every admission bound is a positive finite cap, and the
        // audit floor governs joint purges (it is the stricter horizon).
        assert!(MAX_RSVPS_PER_EVENT > 0);
        assert!(MAX_CHECKINS_PER_OCCURRENCE > 0);
        assert!(MAX_RSVP_WRITES_PER_USER_PER_MINUTE > 0);
        assert!(RSVP_RETENTION_DAYS > 0);
        assert!(AUDIT_RETENTION_DAYS >= RSVP_RETENTION_DAYS);
    }

    #[test]
    fn capacity_and_rate_refusals_name_the_bound_not_the_input() {
        let oversized = "y".repeat(OCCURRENCE_ID_MAX_CHARS + 1);
        for text in [
            rsvp_event_full_text(),
            rsvp_rate_limited_text(),
            checkin_occurrence_full_text(),
        ] {
            assert!(!text.is_empty());
            assert!(!text.contains(&oversized));
        }
        assert!(
            rsvp_event_full_text().contains(&MAX_RSVPS_PER_EVENT.to_string()),
            "capacity text pins the event bound"
        );
        assert!(
            checkin_occurrence_full_text().contains(&MAX_CHECKINS_PER_OCCURRENCE.to_string()),
            "capacity text pins the occurrence bound"
        );
    }

    #[test]
    fn published_set_has_no_attendance_collision() {
        // Scorecard keeps `attendance`; RSVP totals ship namespaced as
        // `rsvp-attendance`. First-wins merge must publish each exactly once.
        let merged = merge_commands(&[feature_commands(), moderation_commands()], &[])
            .expect("slices merge cleanly");
        // 3 core + 16 feature + 9 moderation: nothing deduped away.
        assert_eq!(merged.len(), 28);
        let names: Vec<_> = merged.iter().map(|d| d.name.as_str()).collect();
        for name in ["attendance", "rsvp", "rsvp-attendance"] {
            assert_eq!(
                names.iter().filter(|n| **n == name).count(),
                1,
                "{name} published exactly once"
            );
        }
    }
}
