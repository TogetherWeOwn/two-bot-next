//! Hermetic room-lifecycle fixture tests.
//!
//! Pins the temporary-room lifecycle end to end against the public
//! `voice_rooms` API only (spec `docs/voice-rooms.md` §V1): creator join
//! creates a tracked room, member join/leave drives occupancy decisions,
//! empty-room cleanup removes the tracked row, and reconnect reconciliation
//! diffs tracked rooms against live channels. No database, Redis, Discord,
//! staging guild or production identity is used; the synchronous
//! [`MemRoomStore`] stands in for persistence.

use two_bot_core::voice_rooms::{
    decide_room_join, decide_room_leave, reconcile, ActionQueue, CreatorChannel, MemRoomStore,
    RoomAction, RoomJoinDecision, RoomJoinRequest, RoomLeaveDecision, RoomLeaveReport, RoomStore,
    SeenChannel, VoiceRoom, MAX_CHANNELS_PER_CATEGORY,
};

const GUILD: u64 = 7;
const CREATOR: u64 = 21;
const CATEGORY: u64 = 33;
const OWNER: u64 = 101;
const MEMBER: u64 = 102;

const STAMP: &str = "2026-10-03T00:00:00.000Z";

fn creator() -> CreatorChannel {
    CreatorChannel::new(GUILD, CREATOR)
}

fn join_req(member: u64, seed: u64) -> RoomJoinRequest {
    RoomJoinRequest {
        guild_id: GUILD,
        member_id: member,
        channel_id: CREATOR,
        creator: Some(creator()),
        category_id: Some(CATEGORY),
        category_channel_count: 3,
        seed,
        now: STAMP.to_owned(),
    }
}

fn tracked(channel: u64, owner: u64) -> VoiceRoom {
    VoiceRoom {
        guild_id: GUILD,
        channel_id: channel,
        creator_channel_id: CREATOR,
        owner_id: owner,
        original_creator_id: owner,
        name_seed: 7,
        created_at: STAMP.to_owned(),
    }
}

/// Drive a creator join through spec completion into the store, returning the
/// tracked room as the executor would record it after Discord answers.
fn create_tracked(store: &MemRoomStore, member: u64, seed: u64, channel: u64) -> VoiceRoom {
    let decision = decide_room_join(join_req(member, seed));
    let RoomJoinDecision::CreateRoom { spec } = decision else {
        panic!("creator join must create a room");
    };
    assert_eq!(spec.owner_id, member);
    assert_eq!(spec.creator_channel_id, CREATOR);
    let room = VoiceRoom::from_spec(spec, channel);
    assert_eq!(room.owner_id, room.original_creator_id);
    store.add_room(room.clone());
    room
}

// ---- (1) create ----

#[test]
fn creator_join_creates_exactly_one_tracked_room() {
    let store = MemRoomStore::new();
    let room = create_tracked(&store, OWNER, 1, 501);
    assert_eq!(store.room_for(GUILD, 501), Some(room.clone()));
    assert_eq!(store.rooms_in_guild(GUILD), vec![room]);
}

#[test]
fn non_creator_join_creates_nothing() {
    let store = MemRoomStore::new();
    let mut req = join_req(OWNER, 1);
    req.creator = None;
    assert_eq!(decide_room_join(req), RoomJoinDecision::Ignore);
    assert!(store.rooms_in_guild(GUILD).is_empty());
}

#[test]
fn simultaneous_joins_create_distinct_rooms() {
    let store = MemRoomStore::new();
    let first = create_tracked(&store, OWNER, 1, 501);
    let second = create_tracked(&store, MEMBER, 2, 502);
    assert_ne!(first.channel_id, second.channel_id);
    assert_ne!(first.name_seed, second.name_seed);
    assert_eq!(store.rooms_for_owner(GUILD, OWNER), vec![first]);
    assert_eq!(store.rooms_for_owner(GUILD, MEMBER), vec![second]);
}

#[test]
fn full_category_refuses_creation_without_tracking_a_room() {
    let store = MemRoomStore::new();
    let mut req = join_req(OWNER, 1);
    req.category_channel_count = MAX_CHANNELS_PER_CATEGORY;
    let decision = decide_room_join(req);
    assert!(
        matches!(decision, RoomJoinDecision::RefuseCategoryFull { .. }),
        "full category must refuse, got {decision:?}"
    );
    assert!(store.rooms_in_guild(GUILD).is_empty());
}

// ---- (2) member join/leave ----

#[test]
fn occupied_leave_keeps_the_tracked_room() {
    let store = MemRoomStore::new();
    let room = create_tracked(&store, OWNER, 1, 501);
    assert_eq!(
        decide_room_leave(RoomLeaveReport {
            room: Some(room),
            remaining_humans: 2,
        }),
        RoomLeaveDecision::Ignore
    );
    assert_eq!(store.rooms_in_guild(GUILD).len(), 1);
}

#[test]
fn untracked_leave_touches_nothing() {
    assert_eq!(
        decide_room_leave(RoomLeaveReport {
            room: None,
            remaining_humans: 0,
        }),
        RoomLeaveDecision::Ignore
    );
}

// ---- (3) empty-room cleanup ----

#[test]
fn last_human_leave_deletes_and_untracks_the_room() {
    let store = MemRoomStore::new();
    let room = create_tracked(&store, OWNER, 1, 501);
    let decision = decide_room_leave(RoomLeaveReport {
        room: Some(room.clone()),
        remaining_humans: 0,
    });
    assert_eq!(
        decision,
        RoomLeaveDecision::DeleteRoom { room: room.clone() }
    );
    // The executor deletes the channel, then drops the tracked row.
    assert_eq!(store.remove_room(GUILD, 501), Some(room));
    assert!(store.rooms_in_guild(GUILD).is_empty());
}

#[test]
fn empty_room_cleanup_drains_before_pending_renames() {
    let queue = ActionQueue::new();
    queue.enqueue(
        GUILD,
        RoomAction::RenameRoom {
            channel_id: 501,
            name: "slow".to_owned(),
        },
    );
    queue.enqueue(GUILD, RoomAction::DeleteRoom { channel_id: 502 });
    let first = queue.pop_due(GUILD, 0).expect("cleanup is urgent");
    assert_eq!(first.action, RoomAction::DeleteRoom { channel_id: 502 });
    queue.mark_succeeded(&first);
    let second = queue.pop_due(GUILD, 0).expect("rename follows");
    assert_eq!(
        second.action,
        RoomAction::RenameRoom {
            channel_id: 501,
            name: "slow".to_owned(),
        }
    );
}

// ---- (4) reconcile diff ----

#[test]
fn reconcile_forgets_hand_deleted_rooms_without_a_delete_write() {
    let store = MemRoomStore::new();
    let gone = tracked(501, OWNER);
    store.add_room(gone.clone());
    let plan = reconcile(std::slice::from_ref(&gone), &[]);
    assert_eq!(plan.forget, vec![gone.clone()]);
    assert!(plan.delete_empty.is_empty());
    assert!(plan.suspend.is_empty());
    // Forgetting drops the row; no channel exists, so no delete is queued.
    let queue = ActionQueue::new();
    assert_eq!(store.remove_room(GUILD, 501), Some(gone));
    assert_eq!(queue.drop_for_channel(GUILD, 501), 0);
}

#[test]
fn reconcile_deletes_empty_present_rooms() {
    let room = tracked(502, OWNER);
    let plan = reconcile(
        std::slice::from_ref(&room),
        &[SeenChannel {
            channel_id: 502,
            human_occupants: 0,
            manageable: true,
        }],
    );
    assert!(plan.forget.is_empty());
    assert_eq!(plan.delete_empty, vec![room]);
    assert!(plan.suspend.is_empty());
}

#[test]
fn reconcile_suspends_inaccessible_rooms_and_keeps_them_tracked() {
    let store = MemRoomStore::new();
    let locked = tracked(503, OWNER);
    store.add_room(locked.clone());
    let plan = reconcile(
        std::slice::from_ref(&locked),
        &[SeenChannel {
            channel_id: 503,
            human_occupants: 2,
            manageable: false,
        }],
    );
    assert!(plan.forget.is_empty());
    assert!(plan.delete_empty.is_empty());
    assert_eq!(plan.suspend, vec![locked.clone()]);
    // Suspended rows stay tracked; access loss alone never untracks.
    assert_eq!(store.room_for(GUILD, 503), Some(locked));
}

#[test]
fn reconcile_leaves_occupied_rooms_and_untracked_channels_alone() {
    let lived_in = tracked(504, OWNER);
    let plan = reconcile(
        std::slice::from_ref(&lived_in),
        &[
            SeenChannel {
                channel_id: 504,
                human_occupants: 2,
                manageable: true,
            },
            SeenChannel {
                channel_id: 999,
                human_occupants: 0,
                manageable: true,
            },
        ],
    );
    assert_eq!(plan, Default::default());
    let mentioned: Vec<u64> = plan
        .forget
        .iter()
        .chain(plan.delete_empty.iter())
        .chain(plan.suspend.iter())
        .map(|room| room.channel_id)
        .collect();
    assert!(!mentioned.contains(&504));
    assert!(!mentioned.contains(&999));
}
