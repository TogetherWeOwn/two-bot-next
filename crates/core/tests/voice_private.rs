//! Hermetic V3b acceptance cases against the public privacy core API.

use std::collections::{BTreeSet, VecDeque};

use proptest::prelude::*;
use two_bot_core::voice_private::{
    join_channel_name, ChannelId, EntryOutcome, JoinChannel, JoinDecision, JoinRequest, MemberId,
    PrivacyEffect, PrivacyError, PrivateRoom, RequestId, MAX_CHANNEL_NAME_CHARS,
};

const ROOM: ChannelId = ChannelId(10);
const JOIN: ChannelId = ChannelId(20);
const OWNER: MemberId = MemberId(1);
const ALICE: MemberId = MemberId(2);
const BOB: MemberId = MemberId(3);
const CAROL: MemberId = MemberId(4);

fn public_room() -> PrivateRoom {
    PrivateRoom::new(ROOM, OWNER, "Ana")
}

/// Private with a live Join channel named for the owner.
fn private_room() -> PrivateRoom {
    let room = public_room().make_private().unwrap().room;
    room.join_channel_created(JOIN, "⇩ Join Ana").unwrap().room
}

fn raise(room: &PrivateRoom, member: MemberId) -> (PrivateRoom, JoinRequest) {
    let entry = room.enter_join_channel(JOIN, member, &[OWNER]).unwrap();
    match entry.outcome {
        EntryOutcome::Raised(request) => (entry.plan.room, request),
        other => panic!("expected a new request, got {other:?}"),
    }
}

fn created(id: ChannelId, name: &str) -> Option<JoinChannel> {
    Some(JoinChannel::Created {
        id,
        name: name.to_owned(),
    })
}

// ---- naming ----

#[test]
fn join_channel_is_named_after_the_owner() {
    assert_eq!(join_channel_name("Ana"), "⇩ Join Ana");
    assert_eq!(join_channel_name("  Ana B  "), "⇩ Join Ana B");
    assert_eq!(join_channel_name("   "), "⇩ Join");
    let long = join_channel_name(&"x".repeat(200));
    assert_eq!(long.chars().count(), MAX_CHANNEL_NAME_CHARS);
    assert!(long.starts_with("⇩ Join x"));
    // A cut that ends on a space is trimmed rather than left dangling.
    let spaced = join_channel_name(&format!("{} tail", "y".repeat(91)));
    assert!(!spaced.ends_with(' '));
}

// ---- make_private ----

#[test]
fn new_room_is_public_with_nothing_to_undo() {
    let room = public_room();
    assert!(!room.private);
    assert_eq!(room.join_channel, None);
    assert!(room.blocked.is_empty());
    assert_eq!(room.validate(), Ok(()));
}

#[test]
fn make_private_denies_connect_and_plans_the_join_channel() {
    let room = public_room();
    let plan = room.make_private().unwrap();
    assert_eq!(
        plan.effects,
        vec![
            PrivacyEffect::DenyEveryoneConnect { room_id: ROOM },
            PrivacyEffect::CreateJoinChannel {
                room_id: ROOM,
                name: "⇩ Join Ana".to_owned(),
            },
        ]
    );
    assert!(plan.room.private);
    assert_eq!(plan.room.join_channel, Some(JoinChannel::Requested));
}

#[test]
fn make_private_again_is_a_noop_before_and_after_creation() {
    let requested = public_room().make_private().unwrap().room;
    let again = requested.make_private().unwrap();
    assert!(again.is_noop(&requested));

    let live = private_room();
    assert!(live.make_private().unwrap().is_noop(&live));
}

#[test]
fn make_private_replaces_only_a_lost_join_channel() {
    let lost = private_room().join_channel_deleted(JOIN).unwrap().room;
    assert!(lost.private);
    assert_eq!(lost.join_channel, None);
    let plan = lost.make_private().unwrap();
    assert_eq!(
        plan.effects,
        vec![PrivacyEffect::CreateJoinChannel {
            room_id: ROOM,
            name: "⇩ Join Ana".to_owned(),
        }]
    );
    assert_eq!(plan.room.join_channel, Some(JoinChannel::Requested));
}

// ---- Join channel creation ----

#[test]
fn created_join_channel_is_recorded_once() {
    let requested = public_room().make_private().unwrap().room;
    let plan = requested.join_channel_created(JOIN, "⇩ Join Ana").unwrap();
    assert!(plan.effects.is_empty());
    assert_eq!(plan.room.join_channel, created(JOIN, "⇩ Join Ana"));
    let replay = plan.room.join_channel_created(JOIN, "⇩ Join Ana").unwrap();
    assert!(replay.is_noop(&plan.room));
}

#[test]
fn duplicate_or_orphan_join_channels_are_deleted() {
    let live = private_room();
    let duplicate = live
        .join_channel_created(ChannelId(21), "⇩ Join Ana")
        .unwrap();
    assert_eq!(
        duplicate.effects,
        vec![PrivacyEffect::DeleteJoinChannel {
            channel_id: ChannelId(21)
        }]
    );
    assert_eq!(duplicate.room, live);

    let public = public_room();
    let orphan = public.join_channel_created(ChannelId(21), "x").unwrap();
    assert_eq!(
        orphan.effects,
        vec![PrivacyEffect::DeleteJoinChannel {
            channel_id: ChannelId(21)
        }]
    );
    assert_eq!(orphan.room, public);
}

#[test]
fn going_public_during_creation_deletes_the_channel_on_arrival() {
    let requested = public_room().make_private().unwrap().room;
    let public = requested.make_public().unwrap();
    assert_eq!(
        public.effects,
        vec![PrivacyEffect::RestoreEveryoneConnect { room_id: ROOM }]
    );
    assert_eq!(public.room.join_channel, None);
    let arrived = public
        .room
        .join_channel_created(JOIN, "⇩ Join Ana")
        .unwrap();
    assert_eq!(
        arrived.effects,
        vec![PrivacyEffect::DeleteJoinChannel { channel_id: JOIN }]
    );
    assert_eq!(arrived.room.join_channel, None);
}

#[test]
fn a_name_gone_stale_during_creation_is_renamed() {
    let requested = public_room().make_private().unwrap().room;
    let moved = requested.set_owner(ALICE, "Alice").unwrap();
    // Nothing to rename yet: the channel does not exist.
    assert!(moved.effects.is_empty());
    let arrived = moved.room.join_channel_created(JOIN, "⇩ Join Ana").unwrap();
    assert_eq!(
        arrived.effects,
        vec![PrivacyEffect::RenameJoinChannel {
            channel_id: JOIN,
            name: "⇩ Join Alice".to_owned(),
        }]
    );
    assert_eq!(arrived.room.join_channel, created(JOIN, "⇩ Join Alice"));
}

#[test]
fn creation_failure_and_manual_deletion_are_forgotten_quietly() {
    let requested = public_room().make_private().unwrap().room;
    let failed = requested.join_channel_creation_failed().unwrap();
    assert!(failed.effects.is_empty());
    assert_eq!(failed.room.join_channel, None);
    assert!(failed.room.private);

    let live = private_room();
    assert!(live.join_channel_creation_failed().unwrap().is_noop(&live));
    assert!(live
        .join_channel_deleted(ChannelId(99))
        .unwrap()
        .is_noop(&live));
    let deleted = live.join_channel_deleted(JOIN).unwrap();
    assert!(deleted.effects.is_empty());
    assert_eq!(deleted.room.join_channel, None);
}

#[test]
fn join_channel_ids_are_validated() {
    let requested = public_room().make_private().unwrap().room;
    assert_eq!(
        requested.join_channel_created(ChannelId(0), "x"),
        Err(PrivacyError::InvalidId)
    );
    assert_eq!(
        requested.join_channel_created(ROOM, "x"),
        Err(PrivacyError::JoinChannelIsRoom)
    );
}

// ---- make_public ----

#[test]
fn make_public_restores_access_and_clears_private_state() {
    let room = private_room();
    let (room, _) = raise(&room, ALICE);
    let (room, approved) = raise(&room, BOB);
    let room = room
        .decide(OWNER, approved.id, JoinDecision::Approve)
        .unwrap()
        .room;
    let (room, blocked) = raise(&room, CAROL);
    let room = room
        .decide(OWNER, blocked.id, JoinDecision::Block)
        .unwrap()
        .room;
    let (room, pending) = raise(&room, MemberId(5));
    let alice_request = room.pending[&ALICE];

    let plan = room.make_public().unwrap();
    assert_eq!(
        plan.effects,
        vec![
            PrivacyEffect::RestoreEveryoneConnect { room_id: ROOM },
            PrivacyEffect::RevokeConnect {
                room_id: ROOM,
                member_id: BOB,
            },
            PrivacyEffect::DeleteJoinChannel { channel_id: JOIN },
            PrivacyEffect::WithdrawRequest {
                request: alice_request
            },
            PrivacyEffect::WithdrawRequest { request: pending },
        ]
    );
    assert!(!plan.room.private);
    assert_eq!(plan.room.join_channel, None);
    assert!(plan.room.granted.is_empty());
    assert!(plan.room.pending.is_empty());
    // The block list belongs to the room and survives going public.
    assert_eq!(plan.room.blocked, BTreeSet::from([CAROL]));
    assert!(plan.room.make_public().unwrap().is_noop(&plan.room));
}

#[test]
fn make_public_on_a_public_room_is_a_noop() {
    let room = public_room();
    assert!(room.make_public().unwrap().is_noop(&room));
}

// ---- Join-channel entry ----

#[test]
fn entry_table() {
    let room = private_room();
    let (room, approved) = raise(&room, BOB);
    let room = room
        .decide(OWNER, approved.id, JoinDecision::Approve)
        .unwrap()
        .room;
    let (room, blocked) = raise(&room, CAROL);
    let room = room
        .decide(OWNER, blocked.id, JoinDecision::Block)
        .unwrap()
        .room;
    let occupant = MemberId(6);
    let occupants = [OWNER, occupant];

    let cases = [
        (OWNER, EntryOutcome::HasAccess),
        (occupant, EntryOutcome::HasAccess),
        (BOB, EntryOutcome::HasAccess),
        (CAROL, EntryOutcome::Blocked),
    ];
    for (member, expected) in cases {
        let entry = room.enter_join_channel(JOIN, member, &occupants).unwrap();
        assert_eq!(entry.outcome, expected, "member {member:?}");
        assert!(entry.plan.is_noop(&room), "member {member:?}");
    }
}

#[test]
fn a_blocked_occupant_is_still_silently_ignored() {
    let room = private_room();
    let (room, request) = raise(&room, CAROL);
    let room = room
        .decide(OWNER, request.id, JoinDecision::Block)
        .unwrap()
        .room;
    let entry = room
        .enter_join_channel(JOIN, CAROL, &[OWNER, CAROL])
        .unwrap();
    assert_eq!(entry.outcome, EntryOutcome::Blocked);
    assert!(entry.plan.effects.is_empty());
}

#[test]
fn an_outsider_raises_exactly_one_request() {
    let room = private_room();
    let entry = room.enter_join_channel(JOIN, ALICE, &[OWNER]).unwrap();
    let request = JoinRequest {
        id: RequestId(1),
        member_id: ALICE,
        owner_id: OWNER,
    };
    assert_eq!(entry.outcome, EntryOutcome::Raised(request));
    assert_eq!(
        entry.plan.effects,
        vec![PrivacyEffect::AskOwner { request }]
    );
    assert_eq!(entry.plan.room.pending.len(), 1);
    assert_eq!(entry.plan.room.next_request_id, 2);

    let again = entry
        .plan
        .room
        .enter_join_channel(JOIN, ALICE, &[OWNER])
        .unwrap();
    assert_eq!(again.outcome, EntryOutcome::AlreadyPending(request));
    assert!(again.plan.is_noop(&entry.plan.room));

    let (both, second) = raise(&entry.plan.room, BOB);
    assert_eq!(second.id, RequestId(2));
    assert_eq!(both.pending.len(), 2);
}

#[test]
fn entry_into_a_stale_channel_is_ignored() {
    let public = public_room();
    let requested = public.make_private().unwrap().room;
    let live = private_room();
    for (room, channel) in [(&public, JOIN), (&requested, JOIN), (&live, ChannelId(99))] {
        let entry = room.enter_join_channel(channel, ALICE, &[]).unwrap();
        assert_eq!(entry.outcome, EntryOutcome::StaleChannel);
        assert!(entry.plan.is_noop(room));
    }
}

// ---- decisions ----

#[test]
fn approve_grants_connect_and_moves_the_member() {
    let (room, request) = raise(&private_room(), ALICE);
    let plan = room
        .decide(OWNER, request.id, JoinDecision::Approve)
        .unwrap();
    assert_eq!(
        plan.effects,
        vec![
            PrivacyEffect::GrantConnect {
                room_id: ROOM,
                member_id: ALICE,
            },
            PrivacyEffect::MoveMember {
                member_id: ALICE,
                room_id: ROOM,
            },
        ]
    );
    assert!(plan.room.pending.is_empty());
    assert_eq!(plan.room.granted, BTreeSet::from([ALICE]));
    // A replayed button finds nothing pending.
    assert_eq!(
        plan.room.decide(OWNER, request.id, JoinDecision::Approve),
        Err(PrivacyError::RequestNotPending)
    );
}

#[test]
fn deny_grants_nothing_and_allows_a_new_request() {
    let (room, request) = raise(&private_room(), ALICE);
    let plan = room.decide(OWNER, request.id, JoinDecision::Deny).unwrap();
    assert!(plan.effects.is_empty());
    assert!(plan.room.pending.is_empty());
    assert!(plan.room.granted.is_empty());
    assert!(plan.room.blocked.is_empty());
    let (_, again) = raise(&plan.room, ALICE);
    assert_ne!(again.id, request.id);
}

#[test]
fn block_adds_to_the_block_list_and_stops_requests() {
    let (room, request) = raise(&private_room(), ALICE);
    let plan = room.decide(OWNER, request.id, JoinDecision::Block).unwrap();
    assert!(plan.effects.is_empty());
    assert_eq!(plan.room.blocked, BTreeSet::from([ALICE]));
    assert!(plan.room.pending.is_empty());
    let entry = plan.room.enter_join_channel(JOIN, ALICE, &[OWNER]).unwrap();
    assert_eq!(entry.outcome, EntryOutcome::Blocked);
    assert!(entry.plan.is_noop(&plan.room));
}

#[test]
fn only_the_current_owner_decides() {
    let (room, request) = raise(&private_room(), ALICE);
    for actor in [BOB, ALICE] {
        assert_eq!(
            room.decide(actor, request.id, JoinDecision::Approve),
            Err(PrivacyError::NotOwner)
        );
    }
    assert_eq!(
        room.decide(MemberId(0), request.id, JoinDecision::Approve),
        Err(PrivacyError::InvalidId)
    );
    assert_eq!(
        room.decide(OWNER, RequestId(99), JoinDecision::Approve),
        Err(PrivacyError::RequestNotPending)
    );
}

#[test]
fn a_request_raised_before_an_ownership_change_is_refused() {
    let (room, request) = raise(&private_room(), ALICE);
    let moved = room.set_owner(BOB, "Bob").unwrap().room;
    assert_eq!(
        moved.decide(OWNER, request.id, JoinDecision::Approve),
        Err(PrivacyError::NotOwner)
    );
    assert_eq!(
        moved.decide(BOB, request.id, JoinDecision::Approve),
        Err(PrivacyError::RequestNotPending)
    );
    // Ownership coming back does not revive it either.
    let back = moved.set_owner(OWNER, "Ana").unwrap().room;
    assert_eq!(
        back.decide(OWNER, request.id, JoinDecision::Approve),
        Err(PrivacyError::RequestNotPending)
    );
}

#[test]
fn a_request_raised_before_the_room_went_public_is_refused() {
    let (room, request) = raise(&private_room(), ALICE);
    let public = room.make_public().unwrap().room;
    assert_eq!(
        public.decide(OWNER, request.id, JoinDecision::Approve),
        Err(PrivacyError::NotPrivate)
    );
    let private_again = public.make_private().unwrap().room;
    assert_eq!(
        private_again.decide(OWNER, request.id, JoinDecision::Approve),
        Err(PrivacyError::RequestNotPending)
    );
}

// ---- ownership changes and deletion ----

#[test]
fn privacy_survives_ownership_and_the_join_channel_follows_the_owner() {
    let (room, request) = raise(&private_room(), ALICE);
    let (room, blocked) = raise(&room, CAROL);
    let room = room
        .decide(OWNER, blocked.id, JoinDecision::Block)
        .unwrap()
        .room;
    let plan = room.set_owner(BOB, "Bob").unwrap();
    assert_eq!(
        plan.effects,
        vec![
            PrivacyEffect::WithdrawRequest { request },
            PrivacyEffect::RenameJoinChannel {
                channel_id: JOIN,
                name: "⇩ Join Bob".to_owned(),
            },
        ]
    );
    assert!(plan.room.private);
    assert_eq!(plan.room.owner_id, BOB);
    assert_eq!(plan.room.join_channel, created(JOIN, "⇩ Join Bob"));
    assert!(plan.room.pending.is_empty());
    // Decision: the block list survives ownership changes.
    assert_eq!(plan.room.blocked, BTreeSet::from([CAROL]));
    let entry = plan.room.enter_join_channel(JOIN, CAROL, &[BOB]).unwrap();
    assert_eq!(entry.outcome, EntryOutcome::Blocked);
}

#[test]
fn approved_members_keep_access_across_ownership_changes() {
    let (room, request) = raise(&private_room(), ALICE);
    let room = room
        .decide(OWNER, request.id, JoinDecision::Approve)
        .unwrap()
        .room;
    let moved = room.set_owner(BOB, "Bob").unwrap().room;
    assert_eq!(moved.granted, BTreeSet::from([ALICE]));
}

#[test]
fn renames_happen_only_when_the_name_changes() {
    let room = private_room();
    assert!(room.set_owner(OWNER, "Ana").unwrap().is_noop(&room));
    // Same owner, new `/nick`: rename only, pending requests stay.
    let (room, request) = raise(&room, ALICE);
    let plan = room.set_owner(OWNER, "Annie").unwrap();
    assert_eq!(
        plan.effects,
        vec![PrivacyEffect::RenameJoinChannel {
            channel_id: JOIN,
            name: "⇩ Join Annie".to_owned(),
        }]
    );
    assert_eq!(plan.room.pending[&ALICE], request);
    // A new owner with the same display name needs no rename.
    let twin = plan.room.set_owner(BOB, "Annie").unwrap();
    assert_eq!(
        twin.effects,
        vec![PrivacyEffect::WithdrawRequest { request }]
    );
}

#[test]
fn ownership_change_on_a_public_room_plans_nothing() {
    let room = public_room();
    let plan = room.set_owner(BOB, "Bob").unwrap();
    assert!(plan.effects.is_empty());
    assert_eq!(plan.room.owner_id, BOB);
    assert_eq!(plan.room.owner_display, "Bob");
    assert!(!plan.room.private);
}

#[test]
fn a_blocked_member_who_becomes_owner_stays_blocked_afterwards() {
    let (room, request) = raise(&private_room(), CAROL);
    let room = room
        .decide(OWNER, request.id, JoinDecision::Block)
        .unwrap()
        .room;
    let owned = room.set_owner(CAROL, "Carol").unwrap().room;
    assert_eq!(owned.validate(), Ok(()));
    let entry = owned.enter_join_channel(JOIN, CAROL, &[]).unwrap();
    assert_eq!(entry.outcome, EntryOutcome::HasAccess);
    let handed_back = owned.set_owner(OWNER, "Ana").unwrap().room;
    let entry = handed_back
        .enter_join_channel(JOIN, CAROL, &[OWNER])
        .unwrap();
    assert_eq!(entry.outcome, EntryOutcome::Blocked);
}

#[test]
fn deleting_the_room_plans_deleting_the_join_channel() {
    let (room, request) = raise(&private_room(), ALICE);
    assert_eq!(
        room.delete_room().unwrap(),
        vec![
            PrivacyEffect::WithdrawRequest { request },
            PrivacyEffect::DeleteJoinChannel { channel_id: JOIN },
        ]
    );
    assert_eq!(public_room().delete_room().unwrap(), vec![]);
    let requested = public_room().make_private().unwrap().room;
    assert_eq!(requested.delete_room().unwrap(), vec![]);
}

// ---- invalid state ----

#[test]
fn invalid_states_are_refused_by_every_transition() {
    let mut public_with_channel = public_room();
    public_with_channel.join_channel = created(JOIN, "⇩ Join Ana");

    let mut blocked_and_granted = private_room();
    blocked_and_granted.blocked.insert(ALICE);
    blocked_and_granted.granted.insert(ALICE);

    let (mut foreign_request, _) = raise(&private_room(), ALICE);
    foreign_request.pending.get_mut(&ALICE).unwrap().owner_id = BOB;

    let (mut future_id, _) = raise(&private_room(), ALICE);
    future_id.next_request_id = 1;

    let mut zero_owner = public_room();
    zero_owner.owner_id = MemberId(0);

    let mut room_as_join = private_room();
    room_as_join.join_channel = created(ROOM, "x");

    let cases = [
        (public_with_channel, PrivacyError::PublicRoomHasPrivateState),
        (blocked_and_granted, PrivacyError::BlockedMemberHasAccess),
        (foreign_request, PrivacyError::InconsistentRequest),
        (future_id, PrivacyError::InconsistentRequest),
        (zero_owner, PrivacyError::InvalidId),
        (room_as_join, PrivacyError::JoinChannelIsRoom),
    ];
    for (room, error) in cases {
        assert_eq!(room.validate(), Err(error));
        assert_eq!(room.make_private(), Err(error));
        assert_eq!(room.make_public(), Err(error));
        assert_eq!(room.join_channel_created(ChannelId(30), "x"), Err(error));
        assert_eq!(room.join_channel_creation_failed(), Err(error));
        assert_eq!(room.join_channel_deleted(JOIN), Err(error));
        assert_eq!(room.enter_join_channel(JOIN, BOB, &[]), Err(error));
        assert_eq!(
            room.decide(OWNER, RequestId(1), JoinDecision::Deny),
            Err(error)
        );
        assert_eq!(room.set_owner(BOB, "Bob"), Err(error));
        assert_eq!(room.delete_room(), Err(error));
    }
}

#[test]
fn request_ids_never_wrap() {
    let mut room = private_room();
    room.next_request_id = u64::MAX;
    assert_eq!(
        room.enter_join_channel(JOIN, ALICE, &[]),
        Err(PrivacyError::RequestIdsExhausted)
    );
}

// ---- properties ----

#[derive(Debug, Clone)]
enum Op {
    MakePrivate,
    MakePublic,
    /// The runtime finishes (or fails) the oldest in-flight creation.
    FinishCreation {
        fail: bool,
    },
    DeleteJoinChannelByHand,
    Enter {
        member: u64,
        occupant: bool,
    },
    Decide {
        actor: u64,
        pick: usize,
        decision: JoinDecision,
    },
    SetOwner {
        owner: u64,
        display: u8,
    },
}

fn op() -> impl Strategy<Value = Op> {
    let decision = prop_oneof![
        Just(JoinDecision::Approve),
        Just(JoinDecision::Deny),
        Just(JoinDecision::Block),
    ];
    prop_oneof![
        Just(Op::MakePrivate),
        Just(Op::MakePublic),
        any::<bool>().prop_map(|fail| Op::FinishCreation { fail }),
        Just(Op::DeleteJoinChannelByHand),
        (1..=6u64, any::<bool>()).prop_map(|(member, occupant)| Op::Enter { member, occupant }),
        (1..=6u64, 0..8usize, decision).prop_map(|(actor, pick, decision)| Op::Decide {
            actor,
            pick,
            decision
        }),
        (1..=6u64, 0..3u8).prop_map(|(owner, display)| Op::SetOwner { owner, display }),
    ]
}

/// Join channels that exist on the simulated server, and creations in flight.
#[derive(Default)]
struct World {
    live: BTreeSet<ChannelId>,
    in_flight: VecDeque<String>,
    next_channel: u64,
    raised: Vec<RequestId>,
}

impl World {
    fn apply(&mut self, effects: &[PrivacyEffect]) {
        for effect in effects {
            match effect {
                PrivacyEffect::CreateJoinChannel { name, .. } => {
                    self.in_flight.push_back(name.clone());
                }
                PrivacyEffect::DeleteJoinChannel { channel_id } => {
                    self.live.remove(channel_id);
                }
                PrivacyEffect::RenameJoinChannel { channel_id, .. } => {
                    assert!(self.live.contains(channel_id), "renamed a missing channel");
                }
                PrivacyEffect::AskOwner { request } => self.raised.push(request.id),
                _ => {}
            }
        }
    }

    fn finish_creation(
        &mut self,
        room: &PrivateRoom,
        fail: bool,
    ) -> Option<(PrivateRoom, Vec<PrivacyEffect>)> {
        let name = self.in_flight.pop_front()?;
        let plan = if fail {
            room.join_channel_creation_failed().unwrap()
        } else {
            self.next_channel += 1;
            let id = ChannelId(1_000 + self.next_channel);
            self.live.insert(id);
            room.join_channel_created(id, &name).unwrap()
        };
        Some((plan.room, plan.effects))
    }
}

fn step(
    room: &PrivateRoom,
    world: &mut World,
    op: &Op,
) -> Option<(PrivateRoom, Vec<PrivacyEffect>)> {
    let result = match op {
        Op::MakePrivate => room.make_private(),
        Op::MakePublic => room.make_public(),
        Op::FinishCreation { fail } => return world.finish_creation(room, *fail),
        Op::DeleteJoinChannelByHand => {
            let id = *world.live.iter().next()?;
            world.live.remove(&id);
            room.join_channel_deleted(id)
        }
        Op::Enter { member, occupant } => {
            let channel = match &room.join_channel {
                Some(JoinChannel::Created { id, .. }) => *id,
                _ => ChannelId(999),
            };
            let occupants: Vec<MemberId> =
                occupant.then_some(MemberId(*member)).into_iter().collect();
            room.enter_join_channel(channel, MemberId(*member), &occupants)
                .map(|entry| entry.plan)
        }
        Op::Decide {
            actor,
            pick,
            decision,
        } => {
            let id = world
                .raised
                .get(*pick % world.raised.len().max(1))
                .copied()
                .unwrap_or(RequestId(1));
            room.decide(MemberId(*actor), id, *decision)
        }
        Op::SetOwner { owner, display } => {
            room.set_owner(MemberId(*owner), ["Ana", "Bo", "Cy"][usize::from(*display)])
        }
    };
    // Refusals plan nothing and leave the caller's state as it was.
    result.ok().map(|plan| (plan.room, plan.effects))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// No sequence grants Connect to (or moves in) a member who is blocked
    /// before or after the step, and approved members are never blocked.
    #[test]
    fn property_no_sequence_grants_connect_to_a_blocked_member(
        ops in proptest::collection::vec(op(), 0..48),
    ) {
        let mut room = public_room();
        let mut world = World::default();
        for op in &ops {
            let Some((next, effects)) = step(&room, &mut world, op) else { continue };
            world.apply(&effects);
            for effect in &effects {
                if let PrivacyEffect::GrantConnect { member_id, .. }
                | PrivacyEffect::MoveMember { member_id, .. } = effect
                {
                    prop_assert!(!room.blocked.contains(member_id), "{op:?}: {effect:?}");
                    prop_assert!(!next.blocked.contains(member_id), "{op:?}: {effect:?}");
                }
            }
            prop_assert!(next.granted.is_disjoint(&next.blocked));
            prop_assert!(room.blocked.is_subset(&next.blocked), "blocks are never lifted");
            prop_assert_eq!(next.validate(), Ok(()));
            room = next;
        }
    }

    /// A public room never has a Join channel: not in its state after any
    /// step, and not on the server once every in-flight creation has landed.
    #[test]
    fn property_a_public_room_never_has_a_join_channel(
        ops in proptest::collection::vec(op(), 0..48),
    ) {
        let mut room = public_room();
        let mut world = World::default();
        for op in &ops {
            let Some((next, effects)) = step(&room, &mut world, op) else { continue };
            world.apply(&effects);
            if !next.private {
                prop_assert_eq!(&next.join_channel, &None);
            }
            room = next;
        }
        while let Some((next, effects)) = world.finish_creation(&room, false) {
            world.apply(&effects);
            room = next;
        }
        match &room.join_channel {
            Some(JoinChannel::Created { id, .. }) => {
                prop_assert!(room.private);
                prop_assert_eq!(&world.live, &BTreeSet::from([*id]));
            }
            Some(JoinChannel::Requested) => prop_assert!(false, "creation left pending"),
            None => prop_assert!(world.live.is_empty(), "orphan Join channel {:?}", world.live),
        }
    }
}
