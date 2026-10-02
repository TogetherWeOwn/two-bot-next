//! V2+V3 owner-handoff acceptance: `decide_ownership` into `set_owner`.
//!
//! Pure end-to-end pinning of `docs/voice-rooms.md` V2 (Ownership) + V3 (Join
//! channel follows owner) against the existing public APIs only:
//! `voice_ownership::decide_ownership` decides the handoff, then
//! `PrivateRoom::set_owner` renames the Join channel. No `src` changes.

use std::collections::BTreeSet;

use two_bot_core::voice_ownership::{
    decide_ownership, OwnershipChange, OwnershipDecision, OwnershipError, OwnershipRequest,
    RoomActor, RoomMember, RoomOwnership,
};
use two_bot_core::voice_private::{
    ChannelId, EntryOutcome, JoinChannel, JoinDecision, MemberId, PrivacyEffect, PrivateRoom,
};

const ROOM: ChannelId = ChannelId(10);
const JOIN: ChannelId = ChannelId(20);

const ANA_ID: u64 = 1;
const ALICE_ID: u64 = 2;
const BOB_ID: u64 = 3;
const CAROL_ID: u64 = 4;

const ANA: MemberId = MemberId(ANA_ID);
const ALICE: MemberId = MemberId(ALICE_ID);
const BOB: MemberId = MemberId(BOB_ID);
const CAROL: MemberId = MemberId(CAROL_ID);

const CREATED: RoomOwnership = RoomOwnership {
    owner_id: ANA_ID,
    original_creator_id: ANA_ID,
};

fn human(member_id: u64, joined_at_ms: u64) -> RoomMember {
    RoomMember {
        member_id,
        joined_at_ms,
        is_bot: false,
    }
}

/// Private room with a live Join channel named for Ana (owner 1).
fn private_room() -> PrivateRoom {
    let room = PrivateRoom::new(ROOM, ANA, "Ana")
        .make_private()
        .unwrap()
        .room;
    room.join_channel_created(JOIN, "⇩ Join Ana").unwrap().room
}

fn created(name: &str) -> Option<JoinChannel> {
    Some(JoinChannel::Created {
        id: JOIN,
        name: name.to_owned(),
    })
}

fn changed_next(decision: OwnershipDecision) -> RoomOwnership {
    match decision {
        OwnershipDecision::Changed { next, .. } => next,
        OwnershipDecision::Unchanged(ownership) => ownership,
        OwnershipDecision::EmptyRoom => panic!("expected an occupied room"),
    }
}

fn raise(room: &PrivateRoom, member: MemberId) -> PrivateRoom {
    let entry = room.enter_join_channel(JOIN, member, &[ANA]).unwrap();
    match entry.outcome {
        EntryOutcome::Raised(_) => entry.plan.room,
        other => panic!("expected a new request, got {other:?}"),
    }
}

#[test]
fn caretaker_handoff_renames_join_channel_via_set_owner() {
    // Ana (1) created the room and left; Alice (2) is the earliest joiner.
    let members = [human(ALICE_ID, 20), human(BOB_ID, 30)];
    let decision = decide_ownership(CREATED, &members, OwnershipRequest::Reconcile).unwrap();
    let OwnershipDecision::Changed {
        previous,
        next,
        reason,
    } = decision
    else {
        panic!("expected caretaker change, got {decision:?}");
    };
    assert_eq!(previous, CREATED);
    assert_eq!(reason, OwnershipChange::Caretaker);
    assert_eq!(next.owner_id, ALICE_ID);
    // V2 keeps the original creator across caretaker succession.
    assert_eq!(next.original_creator_id, ANA_ID);

    // V3: the Join channel follows the new owner via set_owner.
    let pending_room = raise(&private_room(), CAROL);
    let plan = pending_room.set_owner(ALICE, "Alice").unwrap();
    let withdraw = plan
        .effects
        .iter()
        .find_map(|effect| match effect {
            PrivacyEffect::WithdrawRequest { request } => Some(*request),
            _ => None,
        })
        .expect("handoff withdraws requests raised to the previous owner");
    assert!(plan.room.pending.is_empty());
    assert_eq!(
        plan.effects,
        vec![
            PrivacyEffect::WithdrawRequest { request: withdraw },
            PrivacyEffect::RenameJoinChannel {
                channel_id: JOIN,
                name: "⇩ Join Alice".to_owned(),
            },
        ]
    );
    assert!(plan.room.private);
    assert_eq!(plan.room.owner_id, ALICE);
    assert_eq!(plan.room.join_channel, created("⇩ Join Alice"));
}

#[test]
fn transfer_handoff_updates_owner_and_creator_and_join_channel_follows() {
    // /transfer from Ana (owner) to Alice (occupant).
    let members = [human(ANA_ID, 10), human(ALICE_ID, 20)];
    let decision = decide_ownership(
        CREATED,
        &members,
        OwnershipRequest::Transfer {
            actor: RoomActor {
                member_id: ANA_ID,
                is_admin: false,
            },
            target_id: ALICE_ID,
        },
    )
    .unwrap();
    let OwnershipDecision::Changed {
        previous,
        next,
        reason,
    } = decision
    else {
        panic!("expected transfer change, got {decision:?}");
    };
    assert_eq!(previous, CREATED);
    assert_eq!(reason, OwnershipChange::Transferred);
    // /transfer replaces both owner and original creator.
    assert_eq!(next.owner_id, ALICE_ID);
    assert_eq!(next.original_creator_id, ALICE_ID);

    // V3 applies the decided owner: the Join channel follows.
    let plan = private_room().set_owner(ALICE, "Alice").unwrap();
    assert_eq!(
        plan.effects,
        vec![PrivacyEffect::RenameJoinChannel {
            channel_id: JOIN,
            name: "⇩ Join Alice".to_owned(),
        }]
    );
    assert!(plan.room.private);
    assert_eq!(plan.room.owner_id, ALICE);
    assert_eq!(plan.room.owner_display, "Alice");
    assert_eq!(plan.room.join_channel, created("⇩ Join Alice"));
}

#[test]
fn reclaim_handoff_restores_creator_and_join_channel_follows_back() {
    // Alice caretakes Ana's room; Ana reclaims while both are present.
    let caretaken = RoomOwnership {
        owner_id: ALICE_ID,
        ..CREATED
    };
    let members = [human(ANA_ID, 30), human(ALICE_ID, 20)];
    let decision = decide_ownership(
        caretaken,
        &members,
        OwnershipRequest::Reclaim { member_id: ANA_ID },
    )
    .unwrap();
    let OwnershipDecision::Changed {
        previous,
        next,
        reason,
    } = decision
    else {
        panic!("expected reclaim change, got {decision:?}");
    };
    assert_eq!(previous, caretaken);
    assert_eq!(reason, OwnershipChange::Reclaimed);
    assert_eq!(next.owner_id, ANA_ID);
    assert_eq!(next.original_creator_id, ANA_ID);

    // V3: the room currently follows Alice; reclaim renames it back to Ana.
    let caretaken_room = private_room().set_owner(ALICE, "Alice").unwrap().room;
    assert_eq!(caretaken_room.join_channel, created("⇩ Join Alice"));
    let plan = caretaken_room.set_owner(ANA, "Ana").unwrap();
    assert_eq!(
        plan.effects,
        vec![PrivacyEffect::RenameJoinChannel {
            channel_id: JOIN,
            name: "⇩ Join Ana".to_owned(),
        }]
    );
    assert_eq!(plan.room.owner_id, ANA);
    assert_eq!(plan.room.join_channel, created("⇩ Join Ana"));
    assert!(plan.room.private);
}

#[test]
fn noncreator_reclaim_is_refused_and_join_channel_stays_put() {
    // Bob is an occupant but not the original creator; Ana still owns the room.
    let members = [human(ANA_ID, 10), human(ALICE_ID, 20), human(BOB_ID, 30)];
    for claimant in [ALICE_ID, BOB_ID] {
        assert_eq!(
            decide_ownership(
                CREATED,
                &members,
                OwnershipRequest::Reclaim {
                    member_id: claimant
                }
            ),
            Err(OwnershipError::NotOriginalCreator)
        );
    }

    // End-to-end: a refused reclaim applies no ownership change, so set_owner
    // is never reached and the Join channel keeps the owner's name.
    let room = private_room();
    let snapshot = room.clone();
    // No decision to apply; the room must be untouched.
    assert_eq!(room, snapshot);
    assert_eq!(room.owner_id, ANA);
    assert_eq!(room.join_channel, created("⇩ Join Ana"));
    assert!(room.set_owner(ANA, "Ana").unwrap().is_noop(&room));
}

#[test]
fn privacy_grants_and_block_list_survive_handoff_end_to_end() {
    // Seed lists: Bob approved (Connect grant), Carol blocked, Alice pending.
    let requested = raise(&private_room(), BOB);
    let approved = requested
        .decide(ANA, requested.pending[&BOB].id, JoinDecision::Approve)
        .unwrap()
        .room;
    let carol_raised = raise(&approved, CAROL);
    let carol_request = carol_raised.pending[&CAROL];
    let with_block = carol_raised
        .decide(ANA, carol_request.id, JoinDecision::Block)
        .unwrap()
        .room;
    let seeded = raise(&with_block, ALICE);
    assert_eq!(seeded.granted, BTreeSet::from([BOB]));
    assert_eq!(seeded.blocked, BTreeSet::from([CAROL]));
    assert!(seeded.pending.contains_key(&ALICE));

    // V2: Ana leaves; earliest joiner Alice (2) becomes caretaker.
    let members = [human(ALICE_ID, 20), human(BOB_ID, 30)];
    let next =
        changed_next(decide_ownership(CREATED, &members, OwnershipRequest::Reconcile).unwrap());
    assert_eq!(next.owner_id, ALICE_ID);
    assert_eq!(next.original_creator_id, ANA_ID);

    // V3: apply the decided owner. Privacy, grant and block list survive;
    // only the stale pending request is withdrawn and the channel renamed.
    let pending_request = seeded.pending[&ALICE];
    let plan = seeded.set_owner(ALICE, "Alice").unwrap();
    assert_eq!(
        plan.effects,
        vec![
            PrivacyEffect::WithdrawRequest {
                request: pending_request
            },
            PrivacyEffect::RenameJoinChannel {
                channel_id: JOIN,
                name: "⇩ Join Alice".to_owned(),
            },
        ]
    );
    assert!(plan.room.private);
    assert_eq!(plan.room.owner_id, ALICE);
    assert_eq!(plan.room.join_channel, created("⇩ Join Alice"));
    assert_eq!(plan.room.granted, BTreeSet::from([BOB]));
    assert_eq!(plan.room.blocked, BTreeSet::from([CAROL]));
    assert!(plan.room.pending.is_empty());
    assert_eq!(plan.room.validate(), Ok(()));
}
