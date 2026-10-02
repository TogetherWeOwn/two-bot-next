//! Community snapshot-builder edge acceptance (TOG-12704).
//!
//! Tests-only slice against the existing public
//! `two_bot_core::community_snapshots` API (`match_rank_roles`,
//! `build_counter_reading`, `build_community_snapshot`, `window_bounds`,
//! `JobGate`; `CounterSkip` / `RankSkip` as the documented caller-side
//! reasons). No `src` changes, no Discord, no database, no REST, no timers,
//! no SQL.
//!
//! Pins the `docs/community-snapshot-edges.md` contract on synthetic rosters:
//! - rank-role matching refuses unladdered, partial and duplicate role sets
//!   (`None`, caller-side [`RankSkip::RankRoleMissing`]) instead of guessing;
//! - counter readings skip malformed rosters (`None`, never a panic) while
//!   succeeding on partial input with explicit exclusion counts;
//! - window bounds refuse malformed ranges (`None`) and never yield a usable
//!   forward window for inverted ranges (caller treats `from >= to` as
//!   ungrounded);
//! - full snapshots succeed on partial input, marking unranked members,
//!   exclusions and nesting explicitly instead of failing the whole snapshot;
//! - the single-flight gate prevents overlapping snapshot cycles.

use two_bot_core::{
    build_community_snapshot, build_counter_reading, match_rank_roles, window_bounds, CounterSkip,
    JobGate, RaidWindow, RankKey, RankRole, RankSkip, RosterMember, RAID_ANOMALIES,
};

fn role_id(key: RankKey) -> String {
    format!("role-{}", key.key())
}

fn ladder() -> Vec<RankRole> {
    RankKey::ALL
        .into_iter()
        .map(|key| RankRole {
            key,
            role_id: role_id(key),
        })
        .collect()
}

fn ladder_names() -> Vec<(String, String)> {
    RankKey::ALL
        .into_iter()
        .map(|key| (role_id(key), key.label().to_owned()))
        .collect()
}

fn member(id: &str, held: &[RankKey], bot: bool) -> RosterMember {
    RosterMember {
        user_id: id.to_owned(),
        is_bot: bot,
        roles: held.iter().map(|key| role_id(*key)).collect(),
    }
}

fn raid_window(id: &str, excluded: &[&str]) -> RaidWindow {
    RaidWindow {
        id: id.to_owned(),
        excluded_member_ids: excluded.iter().map(|entry| (*entry).to_owned()).collect(),
    }
}

#[test]
fn rank_roles_match_exact_ladder_in_order() {
    let matched = match_rank_roles(&ladder_names()).expect("exact ladder matches");
    assert_eq!(
        matched.iter().map(|role| role.key).collect::<Vec<_>>(),
        RankKey::ALL
    );
    // Discord role names vary in case and padding; matching stays exact.
    let padded: Vec<(String, String)> = ladder_names()
        .into_iter()
        .map(|(id, name)| (id, format!("  {}  ", name.to_uppercase())))
        .collect();
    assert!(match_rank_roles(&padded).is_some());
    // Extra unrelated guild roles do not break the ladder.
    let mut with_extra = ladder_names();
    with_extra.push(("role-helpers".to_owned(), "Helpers".to_owned()));
    with_extra.push(("role-mods".to_owned(), "Moderators".to_owned()));
    assert!(match_rank_roles(&with_extra).is_some());
}

#[test]
fn rank_roles_refuse_unladdered_partial_and_duplicate_sets() {
    // Partial ladder: one rung missing, the whole ladder is unusable.
    let names = ladder_names();
    assert!(match_rank_roles(&names[..4]).is_none());
    // Duplicate display name: ambiguous, unusable.
    let mut duplicate = names.clone();
    duplicate.push((
        "role-duplicate".to_owned(),
        RankKey::Prospect.label().to_owned(),
    ));
    assert!(match_rank_roles(&duplicate).is_none());
    // Duplicate of a middle rung is equally ambiguous.
    let mut duplicate_mid = names.clone();
    duplicate_mid.push((
        "role-other-soldier".to_owned(),
        RankKey::Soldier.label().to_owned(),
    ));
    assert!(match_rank_roles(&duplicate_mid).is_none());
    // Unladdered guild: none of the ladder labels present.
    let unladdered = vec![
        ("role-a".to_owned(), "Admins".to_owned()),
        ("role-b".to_owned(), "Moderators".to_owned()),
        ("role-c".to_owned(), "Helpers".to_owned()),
    ];
    assert!(match_rank_roles(&unladdered).is_none());
    // Empty guild: unusable.
    assert!(match_rank_roles(&[]).is_none());
    // Caller-side reason for every refusal above (see website_store tick).
    let _ = RankSkip::RankRoleMissing;
}

#[test]
fn window_bounds_refuse_malformed_ranges() {
    for (start, end) in [
        ("", "2025-07-06"),
        ("2025-07-06", ""),
        ("not-a-date", "2025-07-06"),
        ("2025-07-06", "not-a-date"),
        ("2025-13-01", "2025-13-01"),
        ("2025-00-10", "2025-00-10"),
        ("2025-02-30", "2025-02-30"),
        ("2025-07-06T00:00:00.000Z", "2025-07-06"),
        ("2025/07/06", "2025/07/06"),
        ("2025-07-06-extra", "2025-07-06"),
        (" 2025-07-06", "2025-07-06"),
    ] {
        assert!(window_bounds(start, end).is_none(), "{start}..{end}");
    }
}

#[test]
fn window_bounds_inverted_range_is_never_a_forward_window() {
    // A healthy single-day window is strictly forward.
    let (from, to) = window_bounds("2025-07-06", "2025-07-06").expect("healthy bounds");
    assert!(from < to, "{from}..{to}");
    // Inverted input never yields a forward window: the caller must treat
    // `from >= to` as ungrounded history (CounterSkip::RaidHistoryNotGrounded)
    // and publish nothing.
    for (start, end) in [("2025-07-07", "2025-07-06"), ("2025-07-10", "2025-07-06")] {
        let (from, to) = window_bounds(start, end).expect("parses as dates");
        assert!(from >= to, "{start}..{end} gave {from}..{to}");
    }
    let _ = CounterSkip::RaidHistoryNotGrounded;
}

#[test]
fn window_bounds_ground_static_anomalies_to_forward_windows() {
    assert_eq!(RAID_ANOMALIES.len(), 3);
    for anomaly in RAID_ANOMALIES {
        let (from, to) = window_bounds(anomaly.start, anomaly.end).expect("static window grounds");
        assert!(from < to, "{}", anomaly.id);
    }
    // Month and year boundaries roll over to the next midnight.
    assert_eq!(
        window_bounds("2025-09-12", "2025-09-12").map(|(_, to)| to),
        Some("2025-09-13T00:00:00.000Z".to_owned())
    );
    assert_eq!(
        window_bounds("2024-12-31", "2024-12-31").map(|(_, to)| to),
        Some("2025-01-01T00:00:00.000Z".to_owned())
    );
}

#[test]
fn counter_reading_skips_malformed_rosters_without_panicking() {
    // Empty roster builds nothing.
    assert!(build_counter_reading(&[], &[]).is_none());
    // A blank member id poisons the whole reading, not just one row.
    assert!(build_counter_reading(
        &[RosterMember {
            user_id: String::new(),
            is_bot: false,
            roles: vec![],
        }],
        &[],
    )
    .is_none());
    assert!(build_counter_reading(
        &[
            member("human", &[], false),
            RosterMember {
                user_id: String::new(),
                is_bot: false,
                roles: vec![],
            }
        ],
        &[],
    )
    .is_none());
}

#[test]
fn counter_reading_succeeds_on_partial_input_with_explicit_exclusions() {
    let all: Vec<RankKey> = RankKey::ALL.into();
    let members = vec![
        member("human", &[RankKey::Prospect], false),
        member("unranked", &[], false),
        member("bot", &all, true),
        member("raid", &all, false),
    ];
    let windows = vec![raid_window("raid-window", &["raid"])];
    let reading = build_counter_reading(&members, &windows).expect("partial reading");
    // Bots and raid-window accounts leave the denominator; the exclusion is
    // counted explicitly instead of failing the tick.
    assert_eq!(reading.human_member_count, 2);
    assert_eq!(reading.raid_accounts_excluded, 1);
    // An all-bot roster still builds: zero humans, explicitly.
    let bots = vec![member("bot", &[], true)];
    let idle = build_counter_reading(&bots, &[]).expect("all-bot reading builds");
    assert_eq!(idle.human_member_count, 0);
    assert_eq!(idle.raid_accounts_excluded, 0);
}

#[test]
fn counter_reading_matches_snapshot_denominator() {
    let all: Vec<RankKey> = RankKey::ALL.into();
    let members = vec![
        member("human", &[RankKey::Prospect], false),
        member("bot", &[], true),
        member("raid", &all, false),
    ];
    let windows = vec![raid_window("raid-window", &["raid"])];
    let reading = build_counter_reading(&members, &windows).expect("reading");
    let snapshot =
        build_community_snapshot(&members, &ladder(), &windows).expect("snapshot on same input");
    assert_eq!(snapshot.human_member_count, reading.human_member_count);
    assert_eq!(
        snapshot.raid_accounts_excluded,
        reading.raid_accounts_excluded
    );
}

#[test]
fn snapshot_refuses_malformed_roster_and_ladder_shapes() {
    // Empty or blank roster builds nothing.
    assert!(build_community_snapshot(&[], &ladder(), &[]).is_none());
    assert!(build_community_snapshot(
        &[RosterMember {
            user_id: String::new(),
            is_bot: false,
            roles: vec![],
        }],
        &ladder(),
        &[],
    )
    .is_none());
    let humans = vec![member("human", &[], false)];
    // Short, empty, or misordered ladders build nothing.
    assert!(build_community_snapshot(&humans, &ladder()[..4], &[]).is_none());
    assert!(build_community_snapshot(&humans, &[], &[]).is_none());
    let mut shuffled = ladder();
    shuffled.swap(0, 4);
    assert!(build_community_snapshot(&humans, &shuffled, &[]).is_none());
    let mut extra = ladder();
    extra.push(RankRole {
        key: RankKey::Legend,
        role_id: "role-extra".to_owned(),
    });
    assert!(build_community_snapshot(&humans, &extra, &[]).is_none());
}

#[test]
fn snapshot_succeeds_on_partial_input_with_explicit_marks() {
    let all: Vec<RankKey> = RankKey::ALL.into();
    let snapshot = build_community_snapshot(
        &[
            member("prospect", &[RankKey::Prospect], false),
            member("legend", &all, false),
            member("unranked", &[], false),
            member("raid", &all, false),
            member("bot", &all, true),
        ],
        &ladder(),
        &[raid_window("raid-window", &["raid"])],
    )
    .expect("partial snapshot builds");

    // Bots and raid-window accounts leave the denominator; the raid stay is
    // named explicitly for member_exclusions.
    assert_eq!(snapshot.human_member_count, 3);
    assert_eq!(snapshot.raid_accounts_excluded, 1);
    assert_eq!(snapshot.excluded_member_ids, vec!["raid".to_owned()]);
    // Unranked humans keep their row with an explicit empty rank.
    let unranked = snapshot
        .member_ranks
        .iter()
        .find(|entry| entry.member_id == "unranked")
        .expect("unranked row");
    assert_eq!(unranked.rank_key, None);
    // Highest-rank accounting stays mutually exclusive.
    let prospect = snapshot
        .rank_rows
        .iter()
        .find(|row| row.key == RankKey::Prospect)
        .expect("prospect row");
    assert_eq!(prospect.holders_count, 2);
    assert_eq!(prospect.member_count, 1);
    assert_eq!(snapshot.ranked_member_count, 2);
    assert!(snapshot.nested);
    assert!(snapshot.ranked_member_count <= snapshot.human_member_count);
}

#[test]
fn snapshot_marks_non_nested_ladder_without_failing() {
    // A higher rank without every lower rung still builds; the snapshot
    // carries nested=false and the publish layer maps it to
    // RankSkip::RanksNotNested (writes nothing) instead of publishing.
    let snapshot = build_community_snapshot(
        &[member(
            "broken",
            &[RankKey::Prospect, RankKey::Soldier],
            false,
        )],
        &ladder(),
        &[],
    )
    .expect("non-nested snapshot still builds");
    assert!(!snapshot.nested);
    assert_eq!(snapshot.human_member_count, 1);
    let _ = RankSkip::RanksNotNested;
}

#[test]
fn job_gate_prevents_overlapping_snapshot_cycles() {
    let gate = JobGate::default();
    assert!(!gate.is_running());
    let guard = gate.try_acquire().expect("first tick acquires");
    assert!(gate.is_running());
    // An overlapping tick must skip instead of queueing behind the holder.
    assert!(gate.try_acquire().is_none(), "overlap must skip");
    drop(guard);
    assert!(!gate.is_running());
    // The next tick acquires cleanly once the previous guard drops.
    assert!(gate.try_acquire().is_some(), "gate releases after tick");
    assert!(gate.is_running());
}

#[test]
fn skip_reasons_stay_a_stable_contract() {
    // Skips carry reasons; the tick maps each refusal to exactly one of
    // these instead of failing the whole snapshot or guessing.
    assert_ne!(
        CounterSkip::DiscordReadFailed,
        CounterSkip::RaidHistoryNotGrounded
    );
    assert_ne!(
        RankSkip::DiscordReadFailed,
        RankSkip::RaidHistoryNotGrounded
    );
    assert_ne!(RankSkip::RaidHistoryNotGrounded, RankSkip::RankRoleMissing);
    assert_ne!(RankSkip::RankRoleMissing, RankSkip::RanksNotNested);
}
