//! Retirement-guard unit tests for voice open-half reconciliation.
//!
//! Cutover context: retiring the interim voice bot is safe only when our
//! reconciliation never fabricates durations for live traffic and never
//! pairs sessions across members or guilds. These tests pin the
//! no-live-traffic assertions of [`two_bot_core::voice_reconcile`]:
//! still-open sessions stay unresolved, superseded sessions are flagged
//! rather than stamped, corrupt end rows are reported, and every open half
//! appears exactly once with either a duration or a reason.
//!
//! Hermetic: caller-supplied row slices only. No DB, no network, no clock.

use two_bot_core::voice_reconcile::{
    reconcile_voice_halves, HalfEnd, HalfStart, LeaveRow, ResolutionKind, UnresolvableReason,
};

const GUILD: u64 = 1;
const OTHER_GUILD: u64 = 2;
const MEMBER: u64 = 7;
const OTHER_MEMBER: u64 = 8;

fn start(member: u64, at: &str) -> HalfStart {
    HalfStart {
        guild_id: GUILD,
        member_id: member,
        occurred_at: at.to_owned(),
        channel: "ch-a".to_owned(),
    }
}

fn guild_start(guild: u64, member: u64, at: &str) -> HalfStart {
    HalfStart {
        guild_id: guild,
        member_id: member,
        occurred_at: at.to_owned(),
        channel: "ch-a".to_owned(),
    }
}

fn end(
    member: u64,
    at: &str,
    start_known: bool,
    started_at: Option<&str>,
    duration_seconds: Option<f64>,
) -> HalfEnd {
    HalfEnd {
        guild_id: GUILD,
        member_id: member,
        occurred_at: at.to_owned(),
        channel: "ch-a".to_owned(),
        start_known,
        started_at: started_at.map(str::to_owned),
        duration_seconds,
    }
}

fn guild_end(guild: u64, member: u64, at: &str) -> HalfEnd {
    HalfEnd {
        guild_id: guild,
        member_id: member,
        occurred_at: at.to_owned(),
        channel: "ch-a".to_owned(),
        start_known: false,
        started_at: None,
        duration_seconds: None,
    }
}

fn leave(member: u64, at: &str) -> LeaveRow {
    LeaveRow {
        guild_id: GUILD,
        member_id: member,
        occurred_at: at.to_owned(),
    }
}

#[test]
fn empty_inputs_yield_empty_result() {
    let result = reconcile_voice_halves(&[], &[], &[]);
    assert!(result.resolved.is_empty());
    assert!(result.unresolvable.is_empty());
    assert_eq!(result.complete, 0);
    assert_eq!(result.skipped, 0);
}

#[test]
fn live_start_with_no_end_is_still_open_never_resolved() {
    // The member may be in voice right now: report it, never invent a duration.
    let result = reconcile_voice_halves(&[start(MEMBER, "2026-09-20T12:00:00.000Z")], &[], &[]);
    assert!(result.resolved.is_empty());
    assert_eq!(result.complete, 0);
    assert_eq!(result.unresolvable.len(), 1);
    let only = &result.unresolvable[0];
    assert_eq!(only.reason, UnresolvableReason::StillOpen);
    assert_eq!(only.start_at.as_deref(), Some("2026-09-20T12:00:00.000Z"));
    assert!(only.end_at.is_none());
}

#[test]
fn restart_gap_pairs_forgotten_start_with_unknown_end() {
    // Tracker forgot the start (restart loss) but the database still holds it.
    let result = reconcile_voice_halves(
        &[start(MEMBER, "2026-09-20T12:00:00.000Z")],
        &[end(MEMBER, "2026-09-20T12:05:00.000Z", false, None, None)],
        &[],
    );
    assert_eq!(result.complete, 0);
    assert!(result.unresolvable.is_empty());
    assert_eq!(result.resolved.len(), 1);
    let only = &result.resolved[0];
    assert_eq!(only.resolution, ResolutionKind::RestartGap);
    assert_eq!(only.duration_seconds, 300);
    assert_eq!(only.start_at, "2026-09-20T12:00:00.000Z");
    assert_eq!(only.end_at, "2026-09-20T12:05:00.000Z");
}

#[test]
fn server_leave_closes_open_session_with_duration() {
    // Pre-cutover server leave: no end row, but the leave proves presence.
    let result = reconcile_voice_halves(
        &[start(MEMBER, "2026-09-20T12:00:00.000Z")],
        &[],
        &[leave(MEMBER, "2026-09-20T12:07:00.000Z")],
    );
    assert!(result.unresolvable.is_empty());
    assert_eq!(result.resolved.len(), 1);
    let only = &result.resolved[0];
    assert_eq!(only.resolution, ResolutionKind::ServerLeave);
    assert_eq!(only.duration_seconds, 420);
}

#[test]
fn metadata_recompute_uses_started_at_when_duration_unusable() {
    // Known start with an unusable duration falls back to the row's startedAt.
    let result = reconcile_voice_halves(
        &[],
        &[end(
            MEMBER,
            "2026-09-20T12:05:00.000Z",
            true,
            Some("2026-09-20T12:00:00.000Z"),
            None,
        )],
        &[],
    );
    assert!(result.unresolvable.is_empty());
    assert_eq!(result.resolved.len(), 1);
    let only = &result.resolved[0];
    assert_eq!(only.resolution, ResolutionKind::MetadataRecompute);
    assert_eq!(only.duration_seconds, 300);
    assert!(only.note.is_none());
}

#[test]
fn clock_skew_clamps_to_zero_with_note() {
    // End stamped before the recorded start: clamp, never go negative.
    let result = reconcile_voice_halves(
        &[],
        &[end(
            MEMBER,
            "2026-09-20T12:05:00.000Z",
            true,
            Some("2026-09-20T12:10:00.000Z"),
            None,
        )],
        &[],
    );
    assert_eq!(result.resolved.len(), 1);
    let only = &result.resolved[0];
    assert_eq!(only.resolution, ResolutionKind::MetadataRecompute);
    assert_eq!(only.duration_seconds, 0);
    assert!(only.note.is_some());
}

#[test]
fn superseded_start_is_flagged_never_stamped() {
    // Two starts before any end: the earlier session ended sometime before
    // the later start, but the instant is unknowable — never stamp it.
    let result = reconcile_voice_halves(
        &[
            start(MEMBER, "2026-09-20T12:00:00.000Z"),
            start(MEMBER, "2026-09-20T12:10:00.000Z"),
        ],
        &[],
        &[],
    );
    assert!(result.resolved.is_empty());
    assert_eq!(result.unresolvable.len(), 2);
    let superseded = result
        .unresolvable
        .iter()
        .find(|u| u.reason == UnresolvableReason::Superseded)
        .expect("earlier start is superseded");
    assert_eq!(
        superseded.start_at.as_deref(),
        Some("2026-09-20T12:00:00.000Z")
    );
    assert_eq!(
        superseded.end_at.as_deref(),
        Some("2026-09-20T12:10:00.000Z")
    );
    let live = result
        .unresolvable
        .iter()
        .find(|u| u.reason == UnresolvableReason::StillOpen)
        .expect("later start stays open");
    assert_eq!(live.start_at.as_deref(), Some("2026-09-20T12:10:00.000Z"));
}

#[test]
fn bad_end_row_with_known_start_but_nothing_to_recompute() {
    // Claims a known start yet carries no usable duration, no startedAt,
    // and no start row predates it: report, never synthesize.
    let result = reconcile_voice_halves(
        &[],
        &[end(MEMBER, "2026-09-20T12:05:00.000Z", true, None, None)],
        &[],
    );
    assert!(result.resolved.is_empty());
    assert_eq!(result.unresolvable.len(), 1);
    assert_eq!(result.unresolvable[0].reason, UnresolvableReason::BadEndRow);
}

#[test]
fn orphan_unknown_end_is_no_start_on_file() {
    let result = reconcile_voice_halves(
        &[],
        &[end(MEMBER, "2026-09-20T12:05:00.000Z", false, None, None)],
        &[],
    );
    assert!(result.resolved.is_empty());
    assert_eq!(result.unresolvable.len(), 1);
    let only = &result.unresolvable[0];
    assert_eq!(only.reason, UnresolvableReason::NoStartOnFile);
    assert!(only.start_at.is_none());
}

#[test]
fn members_never_cross_pair() {
    // A's start must not rescue B's orphan end, and vice versa.
    let result = reconcile_voice_halves(
        &[start(MEMBER, "2026-09-20T12:00:00.000Z")],
        &[end(
            OTHER_MEMBER,
            "2026-09-20T12:05:00.000Z",
            false,
            None,
            None,
        )],
        &[],
    );
    assert!(result.resolved.is_empty());
    assert_eq!(result.unresolvable.len(), 2);
    assert!(result
        .unresolvable
        .iter()
        .any(|u| u.member_id == MEMBER && u.reason == UnresolvableReason::StillOpen));
    assert!(result
        .unresolvable
        .iter()
        .any(|u| u.member_id == OTHER_MEMBER && u.reason == UnresolvableReason::NoStartOnFile));
}

#[test]
fn guilds_never_cross_pair() {
    let result = reconcile_voice_halves(
        &[guild_start(GUILD, MEMBER, "2026-09-20T12:00:00.000Z")],
        &[guild_end(OTHER_GUILD, MEMBER, "2026-09-20T12:05:00.000Z")],
        &[],
    );
    assert!(result.resolved.is_empty());
    assert_eq!(result.unresolvable.len(), 2);
    assert!(result
        .unresolvable
        .iter()
        .any(|u| u.guild_id == GUILD && u.reason == UnresolvableReason::StillOpen));
    assert!(result
        .unresolvable
        .iter()
        .any(|u| u.guild_id == OTHER_GUILD && u.reason == UnresolvableReason::NoStartOnFile));
}

#[test]
fn unparseable_leave_is_skipped_never_paired() {
    let result = reconcile_voice_halves(
        &[start(MEMBER, "2026-09-20T12:00:00.000Z")],
        &[],
        &[LeaveRow {
            guild_id: GUILD,
            member_id: MEMBER,
            occurred_at: "not-a-time".to_owned(),
        }],
    );
    assert_eq!(result.skipped, 1);
    // The start stays open: a corrupt leave proves nothing.
    assert!(result.resolved.is_empty());
    assert_eq!(result.unresolvable.len(), 1);
    assert_eq!(result.unresolvable[0].reason, UnresolvableReason::StillOpen);
}
