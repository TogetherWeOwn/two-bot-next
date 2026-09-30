//! Hermetic V2 acceptance cases against the public core API.

use two_bot_core::voice_ownership::{
    decide_ownership, require_room_owner, OwnershipChange, OwnershipDecision, OwnershipError,
    OwnershipRequest, RoomActor, RoomMember, RoomOwnership,
};

const CREATED: RoomOwnership = RoomOwnership {
    owner_id: 1,
    original_creator_id: 1,
};

fn human(member_id: u64, joined_at_ms: u64) -> RoomMember {
    RoomMember {
        member_id,
        joined_at_ms,
        is_bot: false,
    }
}

fn bot(member_id: u64) -> RoomMember {
    RoomMember {
        is_bot: true,
        ..human(member_id, 0)
    }
}

fn actor(member_id: u64, is_admin: bool) -> RoomActor {
    RoomActor {
        member_id,
        is_admin,
    }
}

fn transfer(member_id: u64, is_admin: bool, target_id: u64) -> OwnershipRequest {
    OwnershipRequest::Transfer {
        actor: actor(member_id, is_admin),
        target_id,
    }
}

fn changed(
    previous: RoomOwnership,
    owner_id: u64,
    original_creator_id: u64,
    reason: OwnershipChange,
) -> OwnershipDecision {
    OwnershipDecision::Changed {
        previous,
        next: RoomOwnership {
            owner_id,
            original_creator_id,
        },
        reason,
    }
}

fn next(decision: OwnershipDecision) -> RoomOwnership {
    match decision {
        OwnershipDecision::Changed { next, .. } => next,
        OwnershipDecision::Unchanged(ownership) => ownership,
        OwnershipDecision::EmptyRoom => panic!("expected an occupied room"),
    }
}

#[test]
fn owner_transfer_replaces_both_owner_and_original_creator() {
    let members = [human(1, 10), human(2, 20)];
    assert_eq!(
        decide_ownership(CREATED, &members, transfer(1, false, 2)),
        Ok(changed(CREATED, 2, 2, OwnershipChange::Transferred))
    );
    assert_eq!(members, [human(1, 10), human(2, 20)]);
}

#[test]
fn admin_can_transfer_in_any_room_without_being_an_occupant() {
    let members = [human(1, 10), human(2, 20)];
    assert_eq!(
        decide_ownership(CREATED, &members, transfer(99, true, 2)),
        Ok(changed(CREATED, 2, 2, OwnershipChange::Transferred))
    );
    assert_eq!(
        require_room_owner(CREATED, &members, actor(99, true)),
        Ok(())
    );
}

#[test]
fn ordinary_member_cannot_transfer_even_when_target_is_valid() {
    assert_eq!(
        decide_ownership(
            CREATED,
            &[human(1, 10), human(2, 20)],
            transfer(2, false, 2)
        ),
        Err(OwnershipError::NotOwner)
    );
}

#[test]
fn original_creator_is_not_owner_after_caretaker_succession() {
    let caretaken = RoomOwnership {
        owner_id: 2,
        ..CREATED
    };
    let members = [human(1, 30), human(2, 20)];
    assert_eq!(
        decide_ownership(caretaken, &members, transfer(1, false, 1)),
        Err(OwnershipError::NotOwner)
    );
    assert_eq!(
        require_room_owner(caretaken, &members, actor(2, false)),
        Ok(())
    );
    assert_eq!(
        require_room_owner(caretaken, &members, actor(1, false)),
        Err(OwnershipError::NotOwner)
    );
}

#[test]
fn transfer_rejects_absent_or_bot_target_even_for_admin() {
    let members = [human(1, 10), bot(3)];
    for is_admin in [false, true] {
        for (target, error) in [
            (2, OwnershipError::TargetNotInRoom),
            (3, OwnershipError::BotTarget),
            (0, OwnershipError::InvalidMemberId),
        ] {
            assert_eq!(
                decide_ownership(CREATED, &members, transfer(1, is_admin, target)),
                Err(error)
            );
        }
    }
}

#[test]
fn absent_nonadmin_and_bot_actor_cannot_transfer() {
    let members = [human(2, 20), bot(3)];
    assert_eq!(
        decide_ownership(CREATED, &members, transfer(1, false, 2)),
        Err(OwnershipError::ActorNotInRoom)
    );
    for is_admin in [false, true] {
        assert_eq!(
            decide_ownership(CREATED, &members, transfer(3, is_admin, 2)),
            Err(OwnershipError::BotActor)
        );
    }
}

#[test]
fn departure_selects_earliest_human_and_preserves_creator() {
    let members = [human(3, 30), bot(4), human(2, 20)];
    assert_eq!(
        decide_ownership(CREATED, &members, OwnershipRequest::Reconcile),
        Ok(changed(CREATED, 2, 1, OwnershipChange::Caretaker))
    );
}

#[test]
fn caretaker_departure_keeps_original_creator_across_successions() {
    let caretaken = RoomOwnership {
        owner_id: 2,
        ..CREATED
    };
    assert_eq!(
        decide_ownership(caretaken, &[human(3, 30)], OwnershipRequest::Reconcile),
        Ok(changed(caretaken, 3, 1, OwnershipChange::Caretaker))
    );
}

#[test]
fn tied_join_times_are_stable_under_snapshot_reordering() {
    for members in [
        [human(3, 20), human(2, 20), human(4, 30)],
        [human(4, 30), human(2, 20), human(3, 20)],
        [human(2, 20), human(3, 20), human(4, 30)],
    ] {
        assert_eq!(
            decide_ownership(CREATED, &members, OwnershipRequest::Reconcile),
            Ok(changed(CREATED, 2, 1, OwnershipChange::Caretaker))
        );
    }
}

#[test]
fn rejoining_member_uses_current_stay_not_historical_join() {
    assert_eq!(
        decide_ownership(
            CREATED,
            &[human(2, 100), human(3, 50)],
            OwnershipRequest::Reconcile
        ),
        Ok(changed(CREATED, 3, 1, OwnershipChange::Caretaker))
    );
}

#[test]
fn present_owner_is_not_displaced_by_earlier_occupant_or_returning_creator() {
    let caretaken = RoomOwnership {
        owner_id: 2,
        ..CREATED
    };
    assert_eq!(
        decide_ownership(
            caretaken,
            &[human(1, 10), human(2, 20)],
            OwnershipRequest::Reconcile
        ),
        Ok(OwnershipDecision::Unchanged(caretaken))
    );
}

#[test]
fn creator_can_reclaim_while_caretaker_is_present() {
    let caretaken = RoomOwnership {
        owner_id: 2,
        ..CREATED
    };
    let members = [human(1, 30), human(2, 20)];
    assert_eq!(
        decide_ownership(
            caretaken,
            &members,
            OwnershipRequest::Reclaim { member_id: 1 }
        ),
        Ok(changed(caretaken, 1, 1, OwnershipChange::Reclaimed))
    );
}

#[test]
fn noncreator_cannot_reclaim_while_owner_is_present() {
    for claimant in [2, 3] {
        assert_eq!(
            decide_ownership(
                CREATED,
                &[human(1, 10), human(2, 20), human(3, 30)],
                OwnershipRequest::Reclaim {
                    member_id: claimant
                }
            ),
            Err(OwnershipError::NotOriginalCreator)
        );
    }
}

#[test]
fn any_human_occupant_can_claim_when_owner_is_absent() {
    let members = [human(2, 20), human(3, 30)];
    assert_eq!(
        decide_ownership(
            CREATED,
            &members,
            OwnershipRequest::Reclaim { member_id: 3 }
        ),
        Ok(changed(CREATED, 3, 1, OwnershipChange::Reclaimed))
    );
}

#[test]
fn reclaim_requires_a_present_human_even_for_original_creator() {
    assert_eq!(
        decide_ownership(
            CREATED,
            &[human(2, 20)],
            OwnershipRequest::Reclaim { member_id: 1 }
        ),
        Err(OwnershipError::ActorNotInRoom)
    );
    assert_eq!(
        decide_ownership(
            CREATED,
            &[human(2, 20), bot(3)],
            OwnershipRequest::Reclaim { member_id: 3 }
        ),
        Err(OwnershipError::BotActor)
    );
}

#[test]
fn transferred_creator_can_reclaim_and_former_creator_cannot() {
    let transferred = RoomOwnership {
        owner_id: 3,
        original_creator_id: 2,
    };
    let members = [human(1, 10), human(2, 40), human(3, 30)];
    assert_eq!(
        decide_ownership(
            transferred,
            &members,
            OwnershipRequest::Reclaim { member_id: 1 }
        ),
        Err(OwnershipError::NotOriginalCreator)
    );
    assert_eq!(
        decide_ownership(
            transferred,
            &members,
            OwnershipRequest::Reclaim { member_id: 2 }
        ),
        Ok(changed(transferred, 2, 2, OwnershipChange::Reclaimed))
    );
}

#[test]
fn empty_and_bot_only_rooms_return_to_lifecycle_without_inventing_owner() {
    for members in [vec![], vec![bot(9)]] {
        assert_eq!(
            decide_ownership(CREATED, &members, OwnershipRequest::Reconcile),
            Ok(OwnershipDecision::EmptyRoom)
        );
        for request in [
            OwnershipRequest::Reclaim { member_id: 1 },
            transfer(99, true, 9),
        ] {
            assert_eq!(
                decide_ownership(CREATED, &members, request),
                Err(OwnershipError::EmptyRoom)
            );
        }
    }
}

#[test]
fn replayed_departure_is_unchanged_after_caretaker_is_applied() {
    let members = [human(2, 20), human(3, 30)];
    let request = OwnershipRequest::Reconcile;
    let first = decide_ownership(CREATED, &members, request).unwrap();
    assert_eq!(decide_ownership(CREATED, &members, request), Ok(first));
    let applied = next(first);
    assert_eq!(
        decide_ownership(applied, &members, request),
        Ok(OwnershipDecision::Unchanged(applied))
    );
}

#[test]
fn creator_reclaim_replay_is_a_noop() {
    let caretaken = RoomOwnership {
        owner_id: 2,
        ..CREATED
    };
    let members = [human(1, 30), human(2, 20)];
    let request = OwnershipRequest::Reclaim { member_id: 1 };
    let applied = next(decide_ownership(caretaken, &members, request).unwrap());
    assert_eq!(
        decide_ownership(applied, &members, request),
        Ok(OwnershipDecision::Unchanged(applied))
    );
}

#[test]
fn noncreator_claim_replay_does_not_grant_creator_rights() {
    let members = [human(2, 20), human(3, 30)];
    let request = OwnershipRequest::Reclaim { member_id: 3 };
    let applied = next(decide_ownership(CREATED, &members, request).unwrap());
    assert_eq!(applied.original_creator_id, 1);
    assert_eq!(
        decide_ownership(applied, &members, request),
        Err(OwnershipError::NotOriginalCreator)
    );
}

#[test]
fn transfer_replay_cannot_bypass_changed_authority() {
    let members = [human(1, 10), human(2, 20)];
    let request = transfer(1, false, 2);
    let first = decide_ownership(CREATED, &members, request).unwrap();
    assert_eq!(decide_ownership(CREATED, &members, request), Ok(first));
    let applied = next(first);
    assert_eq!(
        decide_ownership(applied, &members, request),
        Err(OwnershipError::NotOwner)
    );
    assert_eq!(
        decide_ownership(applied, &members, transfer(99, true, 2)),
        Ok(OwnershipDecision::Unchanged(applied))
    );
}

#[test]
fn self_transfer_by_caretaker_updates_creator_but_regular_self_transfer_is_noop() {
    let members = [human(1, 10), human(2, 20)];
    assert_eq!(
        decide_ownership(CREATED, &members, transfer(1, false, 1)),
        Ok(OwnershipDecision::Unchanged(CREATED))
    );
    let caretaken = RoomOwnership {
        owner_id: 2,
        ..CREATED
    };
    assert_eq!(
        decide_ownership(caretaken, &members, transfer(2, false, 2)),
        Ok(changed(caretaken, 2, 2, OwnershipChange::Transferred))
    );
}

#[test]
fn invalid_snapshot_rejects_every_request_without_repairing_it() {
    let cases = [
        (
            RoomOwnership {
                owner_id: 0,
                ..CREATED
            },
            vec![human(2, 20)],
            OwnershipError::InvalidMemberId,
        ),
        (
            RoomOwnership {
                original_creator_id: 0,
                ..CREATED
            },
            vec![human(2, 20)],
            OwnershipError::InvalidMemberId,
        ),
        (CREATED, vec![human(0, 20)], OwnershipError::InvalidMemberId),
        (
            CREATED,
            vec![human(2, 20), human(2, 30)],
            OwnershipError::DuplicateMember(2),
        ),
        (
            CREATED,
            vec![human(2, 20), bot(2)],
            OwnershipError::DuplicateMember(2),
        ),
        (
            CREATED,
            vec![bot(1), human(2, 20)],
            OwnershipError::BotOwnership,
        ),
        (
            RoomOwnership {
                owner_id: 2,
                ..CREATED
            },
            vec![bot(1), human(2, 20)],
            OwnershipError::BotOwnership,
        ),
    ];
    for (ownership, members, error) in cases {
        for request in [
            OwnershipRequest::Reconcile,
            OwnershipRequest::Reclaim { member_id: 2 },
            transfer(99, true, 2),
        ] {
            assert_eq!(decide_ownership(ownership, &members, request), Err(error));
        }
        assert_eq!(
            require_room_owner(ownership, &members, actor(99, true)),
            Err(error)
        );
    }
}

#[test]
fn zero_actor_ids_are_rejected_without_changing_room() {
    let members = [human(1, 10), human(2, 20)];
    for request in [
        OwnershipRequest::Reclaim { member_id: 0 },
        transfer(0, true, 2),
    ] {
        assert_eq!(
            decide_ownership(CREATED, &members, request),
            Err(OwnershipError::InvalidMemberId)
        );
    }
}
