//! Sunday Squad scheduled event: the run of occurrences and the Discord bodies.
//!
//! Ports the scheduled-event half of legacy `src/onboarding/anchorEvent.ts`
//! (blob `c8a3be2c`, lines 46-63, 205-226 and 332-370; TWO-66 §5.4). The
//! wall-clock maths already lives in [`crate::onboarding`]
//! ([`zoned_epoch_secs`](crate::onboarding::zoned_epoch_secs),
//! [`next_anchor_occurrence`]); this module strings occurrences together and
//! shapes the bodies for `POST /guilds/{guild}/scheduled-events`.
//!
//! Every input is a spec and an instant the caller hands in. Nothing here
//! reads a clock, the process environment or the network, so each payload is
//! golden-tested against the legacy output in `tests/anchor_event.rs`.
//!
//! The trap is the onboarding one: the series is 20:00 America/New_York, not
//! "every 604 800 seconds". Each occurrence comes from its own local calendar
//! date, so the week US DST ends (1 November 2026) is 169 hours long.

use serde::Serialize;

use crate::funnel::format_iso_millis;
use crate::onboarding::{next_anchor_occurrence, AnchorSpec, SUNDAY_SQUAD};

/// Sidebar description, TWO-66 §5.4 (legacy `SUNDAY_SQUAD.description`),
/// verbatim. Discord caps an event description at 1000 characters.
pub const SUNDAY_SQUAD_DESCRIPTION: &str = "Fall Guys, an hour, every Sunday. It runs whether there's two of us or eight — a party of two still drops into a full public show. Free on PC, PlayStation, Xbox, Switch and Android, and nothing to be rusty at.\n\nDrop in whenever. No sign-up, no need to say you're coming, and if you haven't got it installed there's something we can play in the room itself.";

/// An anchor spec plus the sidebar copy its scheduled event carries. The
/// onboarding [`AnchorSpec`] has no description field (only the welcome reads
/// it), so the pairing lives here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnchorEvent {
    pub spec: AnchorSpec,
    pub description: &'static str,
}

/// The Sunday Squad event (legacy `SUNDAY_SQUAD`).
pub const SUNDAY_SQUAD_EVENT: AnchorEvent = AnchorEvent {
    spec: SUNDAY_SQUAD,
    description: SUNDAY_SQUAD_DESCRIPTION,
};

/// `recurrence_rule.frequency`: 2 = WEEKLY, the only one used.
pub const FREQUENCY_WEEKLY: u8 = 2;
/// `entity_type`: 2 = VOICE.
pub const ENTITY_TYPE_VOICE: u8 = 2;
/// `privacy_level`: 2 = GUILD_ONLY.
pub const PRIVACY_LEVEL_GUILD_ONLY: u8 = 2;

/// Discord's `recurrence_rule` object, fields in legacy key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecurrenceRule {
    pub start: String,
    pub frequency: u8,
    pub interval: u8,
    pub by_weekday: Vec<u8>,
}

/// Body for `POST /guilds/{guild}/scheduled-events` (legacy
/// `ScheduledEventPayload`). Field order is the legacy key order, so the
/// serialized bytes match. `recurrence_rule` is `None` on the one-off
/// fallback cards and is then omitted, not sent as `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScheduledEventPayload {
    pub name: String,
    pub description: String,
    pub channel_id: String,
    pub entity_type: u8,
    pub privacy_level: u8,
    pub scheduled_start_time: String,
    pub scheduled_end_time: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recurrence_rule: Option<RecurrenceRule>,
}

/// Discord's `by_weekday` is Monday = 0; the spec's weekday is Sunday = 0.
/// Legacy hard-codes Sunday as 6; deriving it gives the same answer for the
/// only spec there is.
fn discord_weekday(weekday: u32) -> u8 {
    ((weekday % 7 + 6) % 7) as u8
}

/// The next `count` starts strictly after `now_secs`, as epoch seconds
/// (legacy `occurrencesFrom`). Each is found from the one before by
/// [`next_anchor_occurrence`], never by adding a week.
pub fn occurrences_from(now_secs: i64, count: usize, spec: AnchorSpec) -> Vec<i64> {
    let mut out = Vec::new();
    let mut cursor = now_secs;
    for _ in 0..count {
        cursor = next_anchor_occurrence(cursor, spec);
        out.push(cursor);
    }
    out
}

/// The start to use when creating or repairing the live recurring series
/// (legacy `liveSeriesStartEpoch`). Discord refuses a start in the past:
/// before run 1 this is run 1, afterwards the next occurrence strictly after
/// `now_secs`. Missed runs are not recreated.
pub fn live_series_start_epoch(now_secs: i64, spec: AnchorSpec) -> i64 {
    if now_secs < spec.series_start_epoch {
        spec.series_start_epoch
    } else {
        next_anchor_occurrence(now_secs, spec)
    }
}

/// The weekly series body (legacy `scheduledEventPayload`). Pass
/// `event.spec.series_start_epoch` for the canonical series anchored on run 1,
/// or [`live_series_start_epoch`] when (re)creating one that has begun:
/// anchoring anywhere but an occurrence shifts every later card.
pub fn scheduled_event_payload(event: AnchorEvent, start_epoch: i64) -> ScheduledEventPayload {
    let mut payload = one_off_payload(event, start_epoch);
    payload.recurrence_rule = Some(RecurrenceRule {
        start: payload.scheduled_start_time.clone(),
        frequency: FREQUENCY_WEEKLY,
        interval: 1,
        by_weekday: vec![discord_weekday(event.spec.weekday)],
    });
    payload
}

/// One-off cards for the next `count` occurrences after `now_secs` (legacy
/// `individualEventPayloads`, six by default there). For guilds that refuse
/// `recurrence_rule`: six cards topped up by hand beat one a week wrong.
pub fn individual_event_payloads(
    now_secs: i64,
    count: usize,
    event: AnchorEvent,
) -> Vec<ScheduledEventPayload> {
    occurrences_from(now_secs, count, event.spec)
        .into_iter()
        .map(|start| one_off_payload(event, start))
        .collect()
}

fn one_off_payload(event: AnchorEvent, start_epoch: i64) -> ScheduledEventPayload {
    let spec = event.spec;
    let start_ms = start_epoch.saturating_mul(1000);
    let duration_ms = (spec.duration_minutes as i64).saturating_mul(60_000);
    ScheduledEventPayload {
        name: spec.name.to_owned(),
        description: event.description.to_owned(),
        channel_id: spec.channel_id.to_owned(),
        entity_type: ENTITY_TYPE_VOICE,
        privacy_level: PRIVACY_LEVEL_GUILD_ONLY,
        scheduled_start_time: format_iso_millis(start_ms),
        scheduled_end_time: format_iso_millis(start_ms.saturating_add(duration_ms)),
        recurrence_rule: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_weekday_is_monday_based() {
        assert_eq!(discord_weekday(0), 6);
        assert_eq!(discord_weekday(1), 0);
        assert_eq!(discord_weekday(6), 5);
    }
}
