//! RSVP-to-attendance conversion instrumentation + reminder schedule (TOG-22858).
//!
//! NO-EVIDENCE→TEST slice: no RSVP-to-attendance conversion rates exist
//! anywhere, so reward design cannot be grounded until the measurement exists.
//! This module is the measurement contract as pure, framework-free domain
//! logic (same seam as [`crate::rsvp`]): inputs are plain data, outcomes are
//! plain data, and every rate, variant assignment and reminder-due decision is
//! unit-testable without Discord or Postgres.
//!
//! What this module owns:
//! - the A/B variant ([`AnnouncementVariant`]): `event_entity_with_reminders`
//!   (Scheduled Event entity + day/hour/15-min reminders + bot hooks) vs
//!   `announcement_post_only` (plain announcement post, no entity, no
//!   reminders). Assignment ([`assign_variant`]) is a stable hash of the event
//!   id, so an event always lands in the same arm without stored state.
//! - the conversion report ([`ConversionReport`]): the RSVP cohort — Going
//!   plus Interested RSVPs (from `event_rsvps` as [`RsvpTotals`], see
//!   [`crate::rsvp`]; Declined never counts) — joined against actual
//!   attendance facts (host check-ins in `community_facts` or event-channel
//!   joins observed on the gateway). [`compute_conversion`] intersects the
//!   two id sets and reports the rate in basis points, or `None` when the
//!   cohort is empty (0/0 is undefined, never 0%). Both A/B arms measure the
//!   same cohort so the published numbers stay comparable.
//! - the publish line ([`render_report_text`]): one deterministic line per
//!   event carrying the numbers event-reward design consumes (AC3).
//! - the reminder schedule ([`ReminderKind`], [`reminders_due`]): the
//!   day/hour/15-min reminder leads the vendor guide claims. At most one
//!   reminder is due per call: the tightest lead still covering the remaining
//!   time, so a late-created event or a scheduler restart skips (never
//!   backfills) the wider leads.
//!
//! Deliberately out of scope: reading `event_rsvps` / `community_facts`
//! (the `rsvp_store` slice owns that), observing voice/channel joins on the
//! gateway, sending reminders over Discord, and any persistence. Callers feed
//! this module the RSVP cohort plus the attendance id set and get back the
//! numbers to publish.

use std::collections::HashSet;

use super::rsvp::RsvpTotals;

/// The two arms of the event-announcement A/B (AC2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnnouncementVariant {
    /// Scheduled Event entity with day/hour/15-min reminders and bot hooks.
    EventEntityWithReminders,
    /// Plain announcement post: no entity, no reminders.
    AnnouncementPostOnly,
}

impl AnnouncementVariant {
    /// Wire value for storage and published numbers.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EventEntityWithReminders => "event_entity_with_reminders",
            Self::AnnouncementPostOnly => "announcement_post_only",
        }
    }

    /// Parse a wire value. Anything else is a caller bug, read as absent.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "event_entity_with_reminders" => Some(Self::EventEntityWithReminders),
            "announcement_post_only" => Some(Self::AnnouncementPostOnly),
            _ => None,
        }
    }
}

impl std::fmt::Display for AnnouncementVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Deterministically assign an event to an A/B arm (AC2): FNV-1a 64 over the
/// event id bytes, even hash → entity arm, odd hash → post-only arm. Stable
/// per event id with no stored state; the distribution is approximately even
/// over snowflake-shaped ids, which is all the instrumentation needs.
#[must_use]
pub fn assign_variant(event_id: &str) -> AnnouncementVariant {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in event_id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    if hash.is_multiple_of(2) {
        AnnouncementVariant::EventEntityWithReminders
    } else {
        AnnouncementVariant::AnnouncementPostOnly
    }
}

/// Conversion numbers for one event (AC1 + AC3): how many of the RSVP cohort
/// (Going plus Interested) actually showed up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionReport {
    /// Discord scheduled-event id (snowflake).
    pub event_id: String,
    /// The A/B arm this event ran in.
    pub variant: AnnouncementVariant,
    /// Distinct members holding a Going or Interested RSVP (the RSVP cohort).
    pub cohort: usize,
    /// Distinct members with an attendance fact (check-in or channel join).
    pub attended: usize,
    /// Cohort members who attended (the intersection).
    pub converted: usize,
    /// Conversion rate in basis points (`converted * 10_000 / cohort`);
    /// `None` when the cohort is empty — 0/0 is undefined, never 0%.
    pub rate_bps: Option<u32>,
    /// Converted member ids, sorted ascending for deterministic output.
    pub converted_members: Vec<String>,
}

impl ConversionReport {
    /// Conversion rate as a percentage string (`"42.50%"`), or `"n/a"` when
    /// the RSVP cohort is empty.
    #[must_use]
    pub fn rate_text(&self) -> String {
        self.rate_bps.map_or_else(
            || "n/a".to_owned(),
            |bps| format!("{}.{:02}%", bps / 100, bps % 100),
        )
    }
}

/// Join the RSVP cohort against attendance facts (AC1).
///
/// The cohort is the union of the Going and Interested lists from
/// [`RsvpTotals`] (Declined never counts): taking the typed totals forces the
/// follow-up wiring to pass both lists, so a Going member who attends counts
/// as converted instead of as a walk-in. Both id sets dedupe: a member counts
/// once no matter how many times they RSVP'd or checked in (a member present
/// in both Going and Interested still counts once). Attendance from members
/// outside the cohort still counts in `attended` (walk-ins are real
/// attendance) but never in `converted` — the rate measures the cohort only.
#[must_use]
pub fn compute_conversion(
    event_id: &str,
    variant: AnnouncementVariant,
    totals: &RsvpTotals,
    attended: &[String],
) -> ConversionReport {
    let cohort_set: HashSet<&str> = totals
        .going
        .iter()
        .chain(totals.interested.iter())
        .map(String::as_str)
        .collect();
    let attended_set: HashSet<&str> = attended.iter().map(String::as_str).collect();
    let mut converted_members: Vec<String> = cohort_set
        .intersection(&attended_set)
        .map(|member| (*member).to_owned())
        .collect();
    converted_members.sort();
    let converted = converted_members.len();
    let cohort_count = cohort_set.len();
    let rate_bps = if cohort_count == 0 {
        None
    } else {
        // `converted <= cohort`, so `converted * 10_000` cannot overflow u64.
        Some((converted as u64 * 10_000 / cohort_count as u64) as u32)
    };
    ConversionReport {
        event_id: event_id.to_owned(),
        variant,
        cohort: cohort_count,
        attended: attended_set.len(),
        converted,
        rate_bps,
        converted_members,
    }
}

/// One deterministic publish line per event carrying the conversion numbers
/// event-reward design consumes (AC3).
///
/// Format: `Event <id> [<variant>]: <converted>/<cohort> rsvp attended
/// (<rate>); <attended> total attended`. Fixed so dashboards and follow-up
/// cards can parse it back.
#[must_use]
pub fn render_report_text(report: &ConversionReport) -> String {
    format!(
        "Event {} [{}]: {}/{} rsvp attended ({}); {} total attended",
        report.event_id,
        report.variant.as_str(),
        report.converted,
        report.cohort,
        report.rate_text(),
        report.attended,
    )
}

/// One of the day/hour/15-min reminder leads the vendor guide claims for the
/// Scheduled Event entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReminderKind {
    /// 24 hours before start.
    Day,
    /// 1 hour before start.
    Hour,
    /// 15 minutes before start.
    QuarterHour,
}

impl ReminderKind {
    /// All three leads, largest lead first (send order).
    pub const ALL: [Self; 3] = [Self::Day, Self::Hour, Self::QuarterHour];

    /// Wire value for storage and logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Hour => "hour",
            Self::QuarterHour => "quarter_hour",
        }
    }

    /// Parse a wire value. Anything else is a caller bug, read as absent.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "day" => Some(Self::Day),
            "hour" => Some(Self::Hour),
            "quarter_hour" => Some(Self::QuarterHour),
            _ => None,
        }
    }

    /// Lead time in milliseconds.
    #[must_use]
    pub fn lead_ms(self) -> i64 {
        match self {
            Self::Day => 24 * 60 * 60 * 1000,
            Self::Hour => 60 * 60 * 1000,
            Self::QuarterHour => 15 * 60 * 1000,
        }
    }
}

/// Which reminder is due now (entity arm only; the post-only arm sends none).
/// Returns at most one reminder: the tightest lead still covering the
/// remaining time (`start - now <= lead`, boundary-inclusive, so an event
/// starting in exactly 1 hour yields the hour reminder). Wider leads are
/// treated as skipped, never backfilled: an event first seen 30 minutes out
/// yields only the hour reminder, never a stale "starts in 24 hours" burst
/// after a late create or scheduler restart. Callers must remember which kind
/// they already sent (the day window re-reports `Day` on every tick until the
/// hour window opens). An event starting now or in the past yields nothing.
/// All inputs are Unix millis; `saturating_sub` keeps extreme clocks from
/// wrapping.
#[must_use]
pub fn reminders_due(event_start_ms: i64, now_ms: i64) -> Vec<ReminderKind> {
    let remaining = event_start_ms.saturating_sub(now_ms);
    if remaining <= 0 {
        return Vec::new();
    }
    ReminderKind::ALL
        .into_iter()
        .filter(|kind| remaining <= kind.lead_ms())
        .min_by_key(|kind| kind.lead_ms())
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rsvp::RsvpTotals;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn totals(going: &[&str], interested: &[&str]) -> RsvpTotals {
        RsvpTotals {
            going: ids(going),
            interested: ids(interested),
            declined: Vec::new(),
        }
    }

    #[test]
    fn variant_round_trips_wire_values() {
        assert_eq!(
            AnnouncementVariant::EventEntityWithReminders.as_str(),
            "event_entity_with_reminders"
        );
        assert_eq!(
            AnnouncementVariant::AnnouncementPostOnly.as_str(),
            "announcement_post_only"
        );
        assert_eq!(
            AnnouncementVariant::parse("event_entity_with_reminders"),
            Some(AnnouncementVariant::EventEntityWithReminders)
        );
        assert_eq!(
            AnnouncementVariant::parse("announcement_post_only"),
            Some(AnnouncementVariant::AnnouncementPostOnly)
        );
        assert_eq!(AnnouncementVariant::parse("entity"), None);
        assert_eq!(AnnouncementVariant::parse(""), None);
    }

    #[test]
    fn variant_assignment_is_stable_per_event() {
        let event = "1546451670500642999";
        assert_eq!(assign_variant(event), assign_variant(event));
        // Both arms are reachable over real-shaped ids (the A/B splits).
        let mut seen_entity = false;
        let mut seen_post = false;
        for suffix in 0..50 {
            match assign_variant(&format!("1546451670500642{suffix:03}")) {
                AnnouncementVariant::EventEntityWithReminders => seen_entity = true,
                AnnouncementVariant::AnnouncementPostOnly => seen_post = true,
            }
        }
        assert!(seen_entity && seen_post, "assignment must split both arms");
    }

    #[test]
    fn conversion_counts_cohort_who_attended() {
        let report = compute_conversion(
            "1546451670500642999",
            AnnouncementVariant::EventEntityWithReminders,
            &totals(&["g1", "g2"], &["u1", "u2", "u3", "u4"]),
            &ids(&["g1", "u2", "u4", "walkin"]),
        );
        assert_eq!(report.cohort, 6);
        assert_eq!(report.attended, 4);
        assert_eq!(report.converted, 3);
        assert_eq!(report.rate_bps, Some(5_000));
        assert_eq!(report.converted_members, ids(&["g1", "u2", "u4"]));
        assert_eq!(report.rate_text(), "50.00%");
    }

    #[test]
    fn conversion_counts_going_attendance_as_converted_not_walkin() {
        let report = compute_conversion(
            "1546451670500642999",
            AnnouncementVariant::EventEntityWithReminders,
            &totals(&["g1"], &["u1"]),
            &ids(&["g1"]),
        );
        assert_eq!(report.cohort, 2);
        assert_eq!(report.attended, 1);
        assert_eq!(report.converted, 1);
        assert_eq!(report.converted_members, ids(&["g1"]));
        assert_eq!(report.rate_bps, Some(5_000));
    }

    #[test]
    fn conversion_dedupes_repeated_ids() {
        let report = compute_conversion(
            "1546451670500642999",
            AnnouncementVariant::AnnouncementPostOnly,
            &totals(&["u1", "u1"], &["u1", "u2"]),
            &ids(&["u1", "u1", "u1"]),
        );
        assert_eq!(report.cohort, 2);
        assert_eq!(report.attended, 1);
        assert_eq!(report.converted, 1);
        assert_eq!(report.rate_bps, Some(5_000));
    }

    #[test]
    fn conversion_with_empty_cohort_is_undefined_never_zero() {
        let report = compute_conversion(
            "1546451670500642999",
            AnnouncementVariant::EventEntityWithReminders,
            &RsvpTotals::default(),
            &ids(&["walkin"]),
        );
        assert_eq!(report.cohort, 0);
        assert_eq!(report.attended, 1);
        assert_eq!(report.converted, 0);
        assert_eq!(report.rate_bps, None);
        assert_eq!(report.rate_text(), "n/a");
        assert!(report.converted_members.is_empty());
    }

    #[test]
    fn conversion_with_no_attendance_is_zero_percent() {
        let report = compute_conversion(
            "1546451670500642999",
            AnnouncementVariant::EventEntityWithReminders,
            &totals(&["g1"], &["u1"]),
            &[],
        );
        assert_eq!(report.converted, 0);
        assert_eq!(report.rate_bps, Some(0));
        assert_eq!(report.rate_text(), "0.00%");
    }

    #[test]
    fn report_text_carries_variant_and_numbers() {
        let report = compute_conversion(
            "1546451670500642999",
            AnnouncementVariant::EventEntityWithReminders,
            &totals(&["g1", "g2"], &["u1", "u2", "u3", "u4"]),
            &ids(&["g1", "u2", "u4", "walkin"]),
        );
        assert_eq!(
            render_report_text(&report),
            "Event 1546451670500642999 [event_entity_with_reminders]: \
             3/6 rsvp attended (50.00%); 4 total attended"
        );
        let empty = compute_conversion(
            "1546451670500642999",
            AnnouncementVariant::AnnouncementPostOnly,
            &RsvpTotals::default(),
            &[],
        );
        assert_eq!(
            render_report_text(&empty),
            "Event 1546451670500642999 [announcement_post_only]: \
             0/0 rsvp attended (n/a); 0 total attended"
        );
    }

    #[test]
    fn reminder_leads_match_vendor_claim() {
        assert_eq!(ReminderKind::Day.lead_ms(), 24 * 60 * 60 * 1000);
        assert_eq!(ReminderKind::Hour.lead_ms(), 60 * 60 * 1000);
        assert_eq!(ReminderKind::QuarterHour.lead_ms(), 15 * 60 * 1000);
        assert_eq!(ReminderKind::parse("day"), Some(ReminderKind::Day));
        assert_eq!(ReminderKind::parse("hour"), Some(ReminderKind::Hour));
        assert_eq!(
            ReminderKind::parse("quarter_hour"),
            Some(ReminderKind::QuarterHour)
        );
        assert_eq!(ReminderKind::parse("week"), None);
    }

    #[test]
    fn reminders_due_fires_tightest_lead_only() {
        let start = 1_769_000_000_000;
        // A day out: only the day reminder is due.
        assert_eq!(
            reminders_due(start, start - 24 * 60 * 60 * 1000),
            vec![ReminderKind::Day]
        );
        // An hour out: the hour window is tighter than day, so only hour.
        assert_eq!(
            reminders_due(start, start - 60 * 60 * 1000),
            vec![ReminderKind::Hour]
        );
        // Fifteen minutes out: only the quarter-hour reminder is due.
        assert_eq!(
            reminders_due(start, start - 15 * 60 * 1000),
            vec![ReminderKind::QuarterHour]
        );
    }

    #[test]
    fn reminders_due_skips_stale_leads_for_late_seen_events() {
        let start = 1_769_000_000_000;
        // Event first seen 30 minutes out: the day lead is skipped, never
        // backfilled as a stale "starts in 24 hours" burst.
        assert_eq!(
            reminders_due(start, start - 30 * 60 * 1000),
            vec![ReminderKind::Hour]
        );
        // Two hours out: still the day window, hour is not yet open.
        assert_eq!(
            reminders_due(start, start - 2 * 60 * 60 * 1000),
            vec![ReminderKind::Day]
        );
        // Ten minutes out: the quarter-hour window already covers it.
        assert_eq!(
            reminders_due(start, start - 10 * 60 * 1000),
            vec![ReminderKind::QuarterHour]
        );
    }

    #[test]
    fn reminders_due_is_empty_at_or_past_start() {
        let start = 1_769_000_000_000;
        assert!(reminders_due(start, start).is_empty());
        assert!(reminders_due(start, start + 1).is_empty());
        // More than a day out: nothing yet.
        assert!(reminders_due(start, start - 24 * 60 * 60 * 1000 - 1).is_empty());
    }

    #[test]
    fn reminders_due_never_wraps_on_extreme_clocks() {
        assert!(reminders_due(i64::MIN, i64::MAX).is_empty());
        assert_eq!(
            reminders_due(i64::MAX, i64::MAX - 60 * 1000),
            vec![ReminderKind::QuarterHour]
        );
    }
}
