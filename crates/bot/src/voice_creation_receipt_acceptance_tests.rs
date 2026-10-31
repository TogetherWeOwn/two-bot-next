//! Acceptance controls (bot side): nothing downstream may be derived
//! from a rejected creation receipt.
//!
//! The Discord adapters reject a semantically contradictory 2xx
//! creation body (wrong/missing guild, wrong kind, wrong/missing
//! requested parent, zero id, malformed/bodyless) as
//! `RoomHttpError::UnknownOutcome` — see the failing-first matrix in
//! `crates/discord/src/voice_creation_receipt_acceptance_tests.rs`.
//! These controls pin the worker posture that rejection relies on,
//! for all three creation flows:
//!
//! - room: no reservation bind, no persisted room, no creator move, no
//!   compensation delete, and no reservation refund/settlement (the
//!   POST may have landed, so capacity stays held and the create is
//!   never reposted);
//! - companion: no tracked or persisted companion ownership from the
//!   rejected receipt, and no second POST in the same dispatch drive;
//! - Join: no deletable/tracked Join channel and no persisted Join
//!   channel id from the rejected receipt, and no delete derived from
//!   it. The later queue retry/adoption behavior is today's separate
//!   design gap: it is documented, not changed, by this work.
//!
//! Every test here is a `control_`: it passes on the baseline and must
//! stay green. The builder changes no worker behavior; if one of these
//! goes red, the adapter change leaked past its contract.

use super::*;

#[tokio::test]
async fn control_rejected_room_receipt_binds_persists_moves_and_refunds_nothing() {
    let (live, store, http, trace) = fixture();
    http.create_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::UnknownOutcome);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;

    assert_eq!(
        *trace.lock().unwrap(),
        ["create"],
        "one POST and nothing derived from its rejected receipt"
    );
    assert!(
        worker.store.bound.lock().unwrap().is_empty(),
        "no channel may be bound to the reservation"
    );
    assert!(
        worker.store.rooms.lock().unwrap().is_empty(),
        "no room ownership may be persisted"
    );
    assert!(worker.rooms.is_empty(), "no room may be tracked");
    assert!(
        worker.store.settled.lock().unwrap().is_empty(),
        "an uncertain outcome never refunds the durable reservation"
    );
    assert_eq!(worker.store.claims.lock().unwrap().len(), 1);
    assert!(
        !worker.dispatch_one(86_400_000).await,
        "the rejected create is never reposted"
    );
    assert_eq!(*trace.lock().unwrap(), ["create"]);
}

#[tokio::test]
async fn control_rejected_companion_receipt_persists_no_companion_ownership() {
    let (live, store, http, trace) = companion_fixture();
    http.companion_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::UnknownOutcome);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    join(&mut worker, MEMBER);
    // Room create, creator move, then the companion create whose receipt
    // is rejected as an uncertain outcome.
    dispatch(&mut worker, 0).await;
    dispatch(&mut worker, 1).await;
    dispatch(&mut worker, 2).await;

    let calls = trace.lock().unwrap().clone();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("create_companion:500"))
            .count(),
        1,
        "exactly one companion POST in this drive: {calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("add_companion:")),
        "no companion row may be written from a rejected receipt: {calls:?}"
    );
    assert!(
        worker.companions.is_empty(),
        "no companion may be tracked as owned"
    );
    assert!(
        worker.store.companions.lock().unwrap().is_empty(),
        "no companion ownership may be persisted"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("grant:")),
        "no View grant may target a channel from a rejected receipt: {calls:?}"
    );
}

#[tokio::test]
async fn control_rejected_join_receipt_creates_no_tracked_or_deletable_channel() {
    let (live, store, http, trace) = owned_room();
    *http.next_id.lock().unwrap() = 700;
    http.join_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::UnknownOutcome);
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.live.set_empty_grace(Duration::ZERO);

    let reply = worker.apply_privacy(MEMBER, false, "Ana", PrivacyCommand::Private);
    assert!(reply.contains("Making this room private"), "{reply}");
    // First the Connect deny lands, then the Join create is rejected as
    // an uncertain outcome.
    assert!(worker.dispatch_one(0).await);
    assert!(worker.dispatch_one(1).await);

    let calls = trace.lock().unwrap().clone();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("create_join"))
            .count(),
        1,
        "exactly one Join POST so far: {calls:?}"
    );
    assert!(
        worker.join_deletable.is_empty(),
        "no deletion authority may come from a rejected receipt"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("delete:")),
        "nothing may be deleted on the strength of a rejected receipt: {calls:?}"
    );
    let stored = worker.store.privacy.lock().unwrap().get(&500).cloned();
    assert_eq!(
        stored.and_then(|record| record.join_channel_id),
        None,
        "no Join channel id may be persisted from a rejected receipt"
    );
    assert!(
        matches!(
            worker
                .privacy
                .get(&500)
                .and_then(|state| state.join_channel.clone()),
            Some(two_bot_core::voice_private::JoinChannel::Requested)
        ),
        "the Join channel stays merely requested, never recorded as created"
    );
}
