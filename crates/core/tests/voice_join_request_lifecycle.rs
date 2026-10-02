//! Hermetic V3 join-request lifecycle acceptance.
//!
//! Pins the raise → pending → decide → withdraw flow against the public
//! `voice_private::PrivateRoom` API only (spec: `docs/voice-rooms.md` §V3 and
//! `docs/voice-private-core.md`). No database, Redis, Discord, or staging
//! identity is used.

use std::collections::BTreeSet;

use two_bot_core::voice_private::{
    ChannelId, EntryOutcome, JoinDecision, JoinRequest, MemberId, PrivacyEffect, PrivacyError,
    PrivateRoom, RequestId,
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

// ---- (1) raise -> pending; duplicate raise while pending dedupes ----

#[test]
fn raise_creates_a_pending_request_and_duplicate_entry_dedupes() {
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

    // Re-entering while pending deduplicates: same request, no new effects.
    let again = entry
        .plan
        .room
        .enter_join_channel(JOIN, ALICE, &[OWNER])
        .unwrap();
    assert_eq!(again.outcome, EntryOutcome::AlreadyPending(request));
    assert!(again.plan.is_noop(&entry.plan.room));
    assert_eq!(again.plan.room.pending.len(), 1);
}

// ---- (2) owner answer clears pending with the correct effects ----

#[test]
fn owner_accept_clears_pending_with_grant_and_move() {
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
}

#[test]
fn owner_deny_clears_pending_with_no_grant() {
    let (room, request) = raise(&private_room(), ALICE);
    let plan = room.decide(OWNER, request.id, JoinDecision::Deny).unwrap();
    assert!(plan.effects.is_empty());
    assert!(plan.room.pending.is_empty());
    assert!(plan.room.granted.is_empty());
    // A denied member may request again with a fresh request ID.
    let (_, again) = raise(&plan.room, ALICE);
    assert_ne!(again.id, request.id);
}

// ---- (3) non-owner answer is refused ----

#[test]
fn non_owner_answer_is_refused_and_leaves_pending_intact() {
    let (room, request) = raise(&private_room(), ALICE);
    for actor in [ALICE, BOB, CAROL] {
        assert_eq!(
            room.decide(actor, request.id, JoinDecision::Approve),
            Err(PrivacyError::NotOwner)
        );
    }
    assert_eq!(room.pending.len(), 1);
    assert_eq!(room.pending[&ALICE], request);
}

// ---- (4) answering a non-pending request is refused ----

#[test]
fn answering_a_non_pending_request_is_refused() {
    let room = private_room();
    assert_eq!(
        room.decide(OWNER, RequestId(1), JoinDecision::Approve),
        Err(PrivacyError::RequestNotPending)
    );
    // An already-answered request is no longer pending either.
    let (room, request) = raise(&room, ALICE);
    let room = room
        .decide(OWNER, request.id, JoinDecision::Deny)
        .unwrap()
        .room;
    assert_eq!(
        room.decide(OWNER, request.id, JoinDecision::Approve),
        Err(PrivacyError::RequestNotPending)
    );
}

// ---- (5) owner transfer or room delete withdraws all pending ----

#[test]
fn owner_transfer_withdraws_all_pending_requests() {
    let (room, first) = raise(&private_room(), ALICE);
    let (room, second) = raise(&room, BOB);
    let plan = room.set_owner(CAROL, "Carol").unwrap();
    assert_eq!(
        plan.effects,
        vec![
            PrivacyEffect::WithdrawRequest { request: first },
            PrivacyEffect::WithdrawRequest { request: second },
            PrivacyEffect::RenameJoinChannel {
                channel_id: JOIN,
                name: "⇩ Join Carol".to_owned(),
            },
        ]
    );
    assert!(plan.room.pending.is_empty());
    // Stale buttons bound to the old requests match nothing now.
    assert_eq!(
        plan.room.decide(CAROL, first.id, JoinDecision::Approve),
        Err(PrivacyError::RequestNotPending)
    );
    // The new owner decides fresh requests.
    let (room, fresh) = raise(&plan.room, ALICE);
    assert_eq!(fresh.owner_id, CAROL);
    let decided = room.decide(CAROL, fresh.id, JoinDecision::Approve).unwrap();
    assert!(decided.room.pending.is_empty());
    assert_eq!(decided.room.granted, BTreeSet::from([ALICE]));
}

#[test]
fn room_delete_withdraws_all_pending_and_plans_join_channel_deletion() {
    let (room, first) = raise(&private_room(), ALICE);
    let (room, second) = raise(&room, BOB);
    assert_eq!(
        room.delete_room().unwrap(),
        vec![
            PrivacyEffect::WithdrawRequest { request: first },
            PrivacyEffect::WithdrawRequest { request: second },
            PrivacyEffect::DeleteJoinChannel { channel_id: JOIN },
        ]
    );
    assert_eq!(public_room().delete_room().unwrap(), vec![]);
}

// ---- (6) join-channel entry while pending routes through decide() ----

#[test]
fn entry_while_pending_never_bypasses_decide() {
    let (room, request) = raise(&private_room(), ALICE);
    // Repeat entries dedupe and never grant or move on their own.
    for _ in 0..2 {
        let entry = room.enter_join_channel(JOIN, ALICE, &[OWNER]).unwrap();
        assert_eq!(entry.outcome, EntryOutcome::AlreadyPending(request));
        assert!(entry.plan.is_noop(&room));
        assert!(entry.plan.room.granted.is_empty());
    }
    // Only decide() resolves the pending state: approve grants and moves.
    let decided = room
        .decide(OWNER, request.id, JoinDecision::Approve)
        .unwrap()
        .room;
    assert!(decided.pending.is_empty());
    // An approved member enters with access and raises no further request.
    let entry = decided.enter_join_channel(JOIN, ALICE, &[OWNER]).unwrap();
    assert_eq!(entry.outcome, EntryOutcome::HasAccess);
    assert!(entry.plan.is_noop(&decided));
}
