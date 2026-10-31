//! Acceptance tests: invite-attribution tracker store round-trip + referral seam.
//!
//! Failing-first spec for the parent invite-attribution build. These tests are
//! RED on the current head: they name the two store-level gaps the Builder
//! closes on the parent card, and they compile + pass once the gaps close.
//!
//! * (a) `InviteState` must carry `max_uses: u64` and `temporary: bool` (the
//!   twilight `InviteCreate` payload already carries both; the tracker drops
//!   them). The literal below is the spec: it fails to compile until the
//!   fields exist, then round-trips unchanged.
//! * (f) `two_bot_core::invites::referral_counts` must aggregate credited
//!   joins per inviter over recorded funnel rows, so the referral-rewards
//!   consumer has a counts seam to read. Pure function over `&[FunnelEvent]`:
//!   count `MemberJoin` rows by their `metadata["inviterId"]`; ignore rows
//!   without a parseable inviter id and non-join rows. The call below is the
//!   spec: it fails to compile until the seam exists.
//!
//! No staging/prod data, no secrets, no wall-clock dependence.

use std::collections::HashMap;

use two_bot_core::invites::{referral_counts, InviteState, InviteTracker, MemSnapshots};
use two_bot_core::{EventType, FunnelEvent, InviteSnapshotStore};

const GUILD: u64 = 100_000_000_000_000_001;
const INVITER_X: u64 = 900_000_000_000_007_101;
const INVITER_Y: u64 = 900_000_000_000_007_102;

/// The specified `InviteState` shape: today's `{code, uses, inviter_id,
/// channel_id}` plus the retained `max_uses`/`temporary` from invite creation.
/// RED: fails to compile until the Builder adds both fields (u64/bool, matching
/// the twilight `InviteCreate` payload).
fn seeded() -> InviteState {
    InviteState {
        code: "fresh01".to_owned(),
        uses: 0,
        inviter_id: Some(7),
        channel_id: Some(8),
        max_uses: 4213,
        temporary: true,
    }
}

/// (a) The creation-time cap and temporariness survive the snapshot store, and
/// a witnessed creation-time baseline still attributes the next join.
#[test]
fn invite_state_carries_max_uses_and_temporary() {
    let snapshots = MemSnapshots::new();
    snapshots.store_all(GUILD, &[seeded()]);
    let loaded = snapshots.load(GUILD);
    assert_eq!(
        loaded,
        vec![seeded()],
        "max_uses/temporary must survive the snapshot store round-trip"
    );

    let tracker = InviteTracker::new(MemSnapshots::new());
    tracker.seed(GUILD, seeded());
    let t0 = 1_700_000_000_000_i64;
    let grew = tracker.diff_and_store_at(
        GUILD,
        &[InviteState {
            uses: 1,
            ..seeded()
        }],
        Some(t0),
    );
    assert_eq!(
        grew,
        vec!["fresh01".to_owned()],
        "seeded baseline measures growth of 1, not of the whole counter"
    );
    assert_eq!(tracker.attribute(&grew, false), "invite:fresh01");
    assert_eq!(tracker.inviter_for(GUILD, "fresh01"), Some(7));
}

fn join(member_id: u64, source: &str, inviter_id: Option<u64>) -> FunnelEvent {
    FunnelEvent {
        guild_id: GUILD,
        member_id: Some(member_id),
        event_type: EventType::MemberJoin,
        occurred_at: "2026-09-20T12:00:00.000Z".to_owned(),
        source: source.to_owned(),
        metadata: inviter_id.map(|id| serde_json::json!({ "inviterId": id.to_string() })),
        dedupe_token: None,
    }
}

/// (f) Per-inviter referral counts aggregate over stored join facts: two joins
/// credited to X and one to Y read back as `{X: 2, Y: 1}`. Unattributed joins
/// and non-join rows contribute nothing. RED: fails to compile until the
/// Builder exposes the `referral_counts` seam.
#[test]
fn referral_counts_aggregates_credited_joins_per_inviter() {
    let events = vec![
        join(1, "invite:a", Some(INVITER_X)),
        join(2, "invite:a", Some(INVITER_X)),
        join(3, "invite:b", Some(INVITER_Y)),
        join(4, "unknown", None),
        join(5, "ambiguous:a+b", None),
        FunnelEvent {
            event_type: EventType::GateCleared,
            source: "gateway".to_owned(),
            metadata: None,
            ..join(1, "gateway", None)
        },
    ];
    let counts: HashMap<u64, u64> = referral_counts(&events);
    assert_eq!(counts.get(&INVITER_X), Some(&2));
    assert_eq!(counts.get(&INVITER_Y), Some(&1));
    assert_eq!(counts.len(), 2, "unattributed joins add no inviter bucket");
}
