//! Voice-reconcile + member-leave-gap integrity acceptance (TOG-12825,
//! obligation TOG-11152).
//!
//! Pure, offline: pins the public `two_bot_core::{voice_reconcile,
//! member_leave_gap}` API only — no gateway, no SQL, no Discord client.
//! Fixtures drive the real `FunnelHandlers` + `MemStore` write path, then
//! project rows through `voice_halves_from_rows` /
//! `leave_gap_feeds_from_rows` exactly as a read-only sweep would, so the
//! checks are proven against the rows the handlers actually persist.
//! Unknown timestamps and memberless rows pass through unmodified and are
//! counted, never synthesized or silently dropped.

use serde_json::json;
use two_bot_core::{
    classify_leave_gaps, leave_gap_feeds_from_rows, reconcile_voice_halves, voice_halves_from_rows,
    EventType, FillBound, FunnelEvent, FunnelHandlers, FunnelStore, GapKind, GapRosterMember,
    JoinInput, MemStore, NoopFacts, NoopLeveling, ResolutionKind, UnresolvableReason, VoiceInput,
    RAID_ANOMALIES,
};

const G: u64 = 1;
const CH_A: u64 = 10;
const CH_B: u64 = 11;

fn handlers() -> FunnelHandlers {
    FunnelHandlers::new(MemStore::new(), Some(NoopLeveling), Some(NoopFacts))
}

fn voice_join(h: &FunnelHandlers, member: u64, channel: u64, at: &str) {
    h.on_voice_join(VoiceInput {
        guild_id: G,
        member_id: member,
        is_bot: false,
        channel_id: channel,
        occurred_at: Some(at.to_owned()),
    });
}

fn voice_leave(h: &FunnelHandlers, member: u64, channel: u64, at: &str) {
    h.on_voice_leave(VoiceInput {
        guild_id: G,
        member_id: member,
        is_bot: false,
        channel_id: channel,
        occurred_at: Some(at.to_owned()),
    });
}

/// Simulate a restart: the in-memory tracker is gone, the rows are not.
fn simulate_restart(h: &FunnelHandlers) {
    h.voice_sessions.lock().expect("voice lock").clear();
}

fn join(h: &FunnelHandlers, member: u64, at: &str) {
    h.on_join(JoinInput {
        guild_id: G,
        member_id: member,
        is_bot: false,
        source: "gateway".to_owned(),
        occurred_at: Some(at.to_owned()),
        inviter_id: None,
        source_event_id: None,
    });
}

fn record_end(
    h: &FunnelHandlers,
    member: u64,
    at: &str,
    start_known: bool,
    started_at: Option<&str>,
) {
    h.store().record(FunnelEvent {
        guild_id: G,
        member_id: Some(member),
        event_type: EventType::VoiceSessionEnd,
        occurred_at: at.to_owned(),
        source: format!("channel:{CH_A}"),
        metadata: Some(json!({
            "startKnown": start_known,
            "startedAt": started_at,
            "durationSeconds": null,
        })),
        dedupe_token: None,
    });
}

#[test]
fn voice_reconcile_resolves_and_flags_every_open_half() {
    let h = handlers();

    // m1: healthy pair through the live path — counted complete, not listed.
    voice_join(&h, 1, CH_A, "2026-09-20T09:00:00.000Z");
    voice_leave(&h, 1, CH_A, "2026-09-20T09:30:00.000Z");

    // m2: restart loss — the tracker forgot the start, the row did not.
    // The join is spelled with an offset; the pairing compares instants but
    // retains the spelling verbatim.
    voice_join(&h, 2, CH_A, "2026-09-20T14:00:00.000+02:00");
    simulate_restart(&h);
    voice_leave(&h, 2, CH_A, "2026-09-20T13:00:00.000Z");

    // m3: pre-TOG-6122 server leave — no end row at all; the leave backstops.
    voice_join(&h, 3, CH_A, "2026-09-20T10:00:00.000Z");
    simulate_restart(&h);
    h.on_leave(G, 3, Some("2026-09-20T10:10:00.000Z".to_owned()), None);

    // m4: nothing after the start — still open (or lost without a trace).
    voice_join(&h, 4, CH_B, "2026-09-20T11:00:00.000Z");

    // m5: two starts, one end — the first is superseded, the end is healthy.
    voice_join(&h, 5, CH_A, "2026-09-20T12:00:00.000Z");
    voice_join(&h, 5, CH_B, "2026-09-20T12:05:00.000Z");
    voice_leave(&h, 5, CH_B, "2026-09-20T12:10:00.000Z");

    // m7: known start, null duration but a valid startedAt — recomputed
    // from the row itself.
    voice_join(&h, 7, CH_A, "2026-09-20T11:00:00.000Z");
    simulate_restart(&h);
    record_end(
        &h,
        7,
        "2026-09-20T11:30:00.000Z",
        true,
        Some("2026-09-20T11:00:00.000Z"),
    );

    // m6: unknown end with no start anywhere on file. The end spelling is
    // retained verbatim in the report.
    record_end(&h, 6, "2026-09-20T17:00:00.000+02:00", false, None);

    // Unknowns pass through unmodified: memberless rows count in the feed
    // adapter, an unparseable start in the pairing.
    h.store().record(FunnelEvent {
        guild_id: G,
        member_id: None,
        event_type: EventType::VoiceSessionStart,
        occurred_at: "2026-09-20T12:00:00.000Z".to_owned(),
        source: format!("channel:{CH_A}"),
        metadata: None,
        dedupe_token: None,
    });
    h.store().record(FunnelEvent {
        guild_id: G,
        member_id: None,
        event_type: EventType::MemberLeave,
        occurred_at: "2026-09-20T12:00:00.000Z".to_owned(),
        source: "gateway".to_owned(),
        metadata: None,
        dedupe_token: None,
    });
    h.store().record(FunnelEvent {
        guild_id: G,
        member_id: Some(999),
        event_type: EventType::VoiceSessionStart,
        occurred_at: "not-a-timestamp".to_owned(),
        source: format!("channel:{CH_A}"),
        metadata: None,
        dedupe_token: None,
    });

    let rows = h.store().rows();
    let feeds = voice_halves_from_rows(&rows);
    assert_eq!(feeds.skipped, 2, "memberless rows are counted, not paired");

    let r = reconcile_voice_halves(&feeds.starts, &feeds.ends, &feeds.leaves);
    assert_eq!(r.complete, 2, "m1 and m5 ends are healthy");
    assert_eq!(r.skipped, 1, "unparseable row counted, never paired");

    // Restart-gap: 14:00+02:00 is 12:00Z; the leave is 13:00Z → 3600 s, and
    // the source spelling is retained, not normalized.
    let m2 = r
        .resolved
        .iter()
        .find(|s| s.member_id == 2)
        .expect("m2 restart-gap");
    assert_eq!(m2.resolution, ResolutionKind::RestartGap);
    assert_eq!(m2.duration_seconds, 3600);
    assert_eq!(m2.start_at, "2026-09-20T14:00:00.000+02:00");

    // Server-leave backstop: 10:00 → 10:10 → 600 s.
    let m3 = r
        .resolved
        .iter()
        .find(|s| s.member_id == 3)
        .expect("m3 server-leave");
    assert_eq!(m3.resolution, ResolutionKind::ServerLeave);
    assert_eq!(
        (m3.start_at.as_str(), m3.end_at.as_str()),
        ("2026-09-20T10:00:00.000Z", "2026-09-20T10:10:00.000Z")
    );
    assert_eq!(m3.duration_seconds, 600);

    // Metadata recompute: 11:00 → 11:30 → 1800 s from the row's own start.
    let m7 = r
        .resolved
        .iter()
        .find(|s| s.member_id == 7)
        .expect("m7 metadata-recompute");
    assert_eq!(m7.resolution, ResolutionKind::MetadataRecompute);
    assert_eq!(m7.duration_seconds, 1800);

    let reasons: Vec<(u64, UnresolvableReason)> = r
        .unresolvable
        .iter()
        .map(|u| (u.member_id, u.reason))
        .collect();
    assert!(
        reasons.contains(&(4, UnresolvableReason::StillOpen)),
        "{reasons:?}"
    );
    assert!(
        reasons.contains(&(5, UnresolvableReason::Superseded)),
        "{reasons:?}"
    );
    assert!(
        reasons.contains(&(6, UnresolvableReason::NoStartOnFile)),
        "{reasons:?}"
    );
    assert_eq!(r.unresolvable.len(), 3);

    // The orphan end keeps its spelling: unknown retained, never rewritten.
    let m6 = r
        .unresolvable
        .iter()
        .find(|u| u.member_id == 6)
        .expect("m6");
    assert_eq!(m6.start_at, None);
    assert_eq!(m6.end_at.as_deref(), Some("2026-09-20T17:00:00.000+02:00"));

    // The superseded bound is honest: at most 5 minutes, never a duration.
    let m5 = r
        .unresolvable
        .iter()
        .find(|u| u.member_id == 5)
        .expect("m5");
    assert!(m5.detail.contains("5m"), "{}", m5.detail);
}

#[test]
fn leave_gap_classifier_partitions_every_joined_member() {
    let h = handlers();

    // Still here with no leave row: correct, counted present.
    join(&h, 101, "2025-06-01T10:00:00.000Z");
    // Join plus leave, gone: resolved.
    join(&h, 102, "2025-05-01T10:00:00.000Z");
    h.on_leave(G, 102, Some("2025-05-10T10:00:00.000Z".to_owned()), None);
    // Last join older than the floor: the leave predates readable history.
    join(&h, 103, "2023-01-15T10:00:00.000Z");
    // Last join inside history with no leave: the logger missed it. Offset
    // spelling retained; comparison is by instant.
    join(&h, 104, "2025-06-15T12:00:00.000+02:00");
    // Joined mid-raid (2025-07-06 window), never seen leaving: residue.
    join(&h, 105, "2025-07-06T21:00:00.000Z");
    // Two joins, no leaves: left between them and after the last one.
    join(&h, 106, "2024-05-01T10:00:00.000Z");
    join(&h, 106, "2024-09-01T10:00:00.000Z");
    // Malformed: skipped, never paired.
    join(&h, 107, "not-a-timestamp");
    // Memberless rows: counted by the feed adapter, never paired.
    h.store().record(FunnelEvent {
        guild_id: G,
        member_id: None,
        event_type: EventType::MemberJoin,
        occurred_at: "2025-06-01T10:00:00.000Z".to_owned(),
        source: "gateway".to_owned(),
        metadata: None,
        dedupe_token: None,
    });
    h.store().record(FunnelEvent {
        guild_id: G,
        member_id: None,
        event_type: EventType::MemberLeave,
        occurred_at: "2025-05-11T10:00:00.000Z".to_owned(),
        source: "gateway".to_owned(),
        metadata: None,
        dedupe_token: None,
    });

    let rows = h.store().rows();
    let (joins, leaves, feed_skipped) = leave_gap_feeds_from_rows(&rows);
    assert_eq!(feed_skipped, 2, "memberless rows counted, never paired");

    let roster = vec![
        GapRosterMember {
            guild_id: G,
            member_id: Some(101),
        },
        GapRosterMember {
            guild_id: G,
            member_id: Some(999),
        },
    ];
    let r = classify_leave_gaps(
        &joins,
        &leaves,
        &roster,
        Some("2024-01-01T00:00:00.000Z"),
        &RAID_ANOMALIES,
    );

    assert_eq!(r.present, 1, "101 still here is correct, not a gap");
    assert_eq!(r.resolved, 1, "102 has its leave row");
    // 107's bad timestamp: counted by the classifier, never paired. The
    // feed adapter already counted the two memberless rows above.
    assert_eq!(r.skipped, 1);
    assert_eq!(r.gaps.len(), 4);

    // Sorted oldest-first by instant: 103, 106, 104, 105.
    let members: Vec<u64> = r.gaps.iter().map(|g| g.member_id).collect();
    assert_eq!(members, vec![103, 106, 104, 105]);

    let kind_of = |m: u64| r.gaps.iter().find(|g| g.member_id == m).map(|g| g.kind);
    assert_eq!(kind_of(103), Some(GapKind::PreCoverage));
    assert_eq!(kind_of(104), Some(GapKind::LogMiss));
    assert_eq!(kind_of(105), Some(GapKind::RaidResidue));
    assert_eq!(kind_of(106), Some(GapKind::RejoinGap));

    // The offset spelling survives into the report; the floor comparison ran
    // on the instant (12:00+02:00 is 10:00Z, inside history → log-miss).
    let m104 = r.gaps.iter().find(|g| g.member_id == 104).expect("104");
    assert_eq!(m104.last_join_at, "2025-06-15T12:00:00.000+02:00");

    // Fills stamp the earlier join, never the later: the inter-join
    // departure would otherwise dedupe against the next fill's key.
    let m106 = r.gaps.iter().find(|g| g.member_id == 106).expect("106");
    assert_eq!(m106.joins_seen, 2);
    assert_eq!(m106.fills.len(), 2);
    assert!(m106
        .fills
        .iter()
        .all(|f| f.bound == FillBound::EarliestPossible));
    assert_eq!(m106.fills[0].occurred_at, "2024-05-01T10:00:00.000Z");
    assert_eq!(m106.fills[1].occurred_at, "2024-09-01T10:00:00.000Z");
}
