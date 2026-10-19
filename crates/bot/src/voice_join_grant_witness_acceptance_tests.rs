//! Acceptance tests: an uncertain join-grant PUT keeps its durable witness
//! through later retry refusals.
//!
//! Approving a join request persists a grant witness (room, member) before
//! the Discord overwrite PUT, so a later `/public` knows whose bot-owned
//! Connect bit to take back. When a PUT attempt reports
//! `RoomHttpError::UnknownOutcome`, the grant may have landed: that witness
//! is a retained access obligation, not a fresh intent. A later retry that
//! fails definitively (`AccessDenied`, `Cancelled`, ...) does not prove the
//! earlier ambiguous PUT failed, so it must not retire the older witness.
//! The witness survives until an authoritative revocation resolves it.
//!
//! Builder contract (this file is preserved verbatim; see the manifest on
//! the acceptance card). The outcomes below are the contract; how fresh
//! intent is distinguished from an existing retained obligation at the
//! persistence boundary is the Builder's choice, as long as:
//!
//! - unknown-then-denied and unknown-then-cancelled grant retries leave
//!   exactly the original stored witness row and the in-memory granted
//!   claim in place;
//! - restart hydration still finds that retained witness, and `/public`
//!   revokes only the bot-owned Connect bit, preserving the member's
//!   unrelated allow/deny bits and other members' denies (including a
//!   vote-kick Connect deny);
//! - an uncertain (`UnknownOutcome`) revocation retains its obligation
//!   until a later authoritative retry retires it;
//! - the two controls stay green: a fresh first-attempt definitive refusal
//!   still unwinds completely (no witness, member may ask again), and a
//!   landed grant whose move fails stays recorded.
//!
//! No new grant/release authority and no inference of ownership from
//! arbitrary Connect allows: ownership comes only from the witness rows.

use super::*;
use crate::voice_rooms::join_requests::JoinClick;
use two_bot_core::voice_private::{JoinDecision, MemberId};

const ROOM: u64 = 500;
const JOIN: u64 = 700;
const OWNER: u64 = MEMBER;
const OUTSIDER: u64 = 401;
const VOTE_KICKED: u64 = 402;

type Worker = GuildRoomWorker<Store, Http>;

/// The room 500 fixture (owner `OWNER`), with fake channel ids starting
/// above the room so a created Join channel never collides.
fn parts() -> (LiveGuild, Store, Http, Trace) {
    let (live, store, http, trace) = owned_room();
    *http.next_id.lock().unwrap() = JOIN;
    (live, store, http, trace)
}

/// Run every queued write to completion; each tick jumps past any backoff.
async fn drain(worker: &mut Worker) {
    for step in 1..=40u64 {
        if !worker.dispatch_one(step * 1_000_000).await {
            return;
        }
    }
    panic!("queue did not drain");
}

/// A private room with its Join channel up and an empty trace.
async fn private_room() -> (Worker, Trace) {
    let (live, store, http, trace) = parts();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.live.set_empty_grace(Duration::ZERO);
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    assert_eq!(worker.join_channel_of(ROOM), Some(JOIN));
    trace.lock().unwrap().clear();
    (worker, trace)
}

fn enter(worker: &Worker, member: u64, at: u64) {
    worker
        .live
        .voice_update_at(member, Some(JOIN), Some(false), at);
}

fn request_id_of(worker: &Worker, member: u64) -> u64 {
    worker.privacy[&ROOM].pending[&MemberId(member)].id.0
}

fn press_approve(worker: &mut Worker, request_id: u64) {
    worker.apply_join_decision(JoinDecisionCommand {
        actor_id: OWNER,
        click: JoinClick {
            decision: JoinDecision::Approve,
            room_id: ROOM,
            request_id,
            epoch: Some(worker.join.epoch),
        },
    });
}

fn stored_grants(worker: &Worker) -> Vec<u64> {
    worker
        .store
        .join_grants
        .lock()
        .unwrap()
        .get(&ROOM)
        .map(|members| members.iter().copied().collect())
        .unwrap_or_default()
}

fn granted(worker: &Worker, member: u64) -> bool {
    worker.privacy[&ROOM].granted.contains(&MemberId(member))
}

/// Script the next overwrite-PUT outcomes, consumed in attempt order.
fn script_overwrites(worker: &Worker, errors: &[RoomHttpError]) {
    let mut queue = worker.http.overwrite_errors.lock().unwrap();
    for error in errors {
        queue.push_back(*error);
    }
}

/// Enter, request, and leave the prompt delivered; returns the request id.
async fn requested(worker: &mut Worker) -> u64 {
    enter(worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(worker).await;
    request_id_of(worker, OUTSIDER)
}

/// A worker for the same guild after a restart: same Discord, same stored
/// room, privacy record and grant witness, nothing else remembered.
async fn restarted(worker: &Worker) -> Worker {
    let trace = Trace::default();
    let store = Store::new(trace.clone());
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    store.privacy.lock().unwrap().insert(
        ROOM,
        worker
            .store
            .privacy
            .lock()
            .unwrap()
            .get(&ROOM)
            .cloned()
            .expect("persisted privacy"),
    );
    for (room_id, members) in worker.store.join_grants.lock().unwrap().iter() {
        store
            .join_grants
            .lock()
            .unwrap()
            .insert(*room_id, members.clone());
    }
    let http = Http::new(trace);
    *http.next_id.lock().unwrap() = JOIN + 50;
    GuildRoomWorker::load(worker.live.clone(), store, http)
        .await
        .unwrap()
}

fn member_overwrite(worker: &Worker, member: u64) -> PermissionOverwrite {
    worker
        .live_overwrites(ROOM)
        .unwrap()
        .into_iter()
        .find(|overwrite| {
            overwrite.kind == PermissionOverwriteType::Member && overwrite.id.get() == member
        })
        .expect("member overwrite entry")
}

#[tokio::test]
async fn join_grant_witness_unknown_then_denied_retains_the_witness() {
    let (mut worker, _trace) = private_room().await;
    let request = requested(&mut worker).await;
    // The first PUT may have landed; the retry is refused. The refusal says
    // nothing about the first attempt, so the witness must survive.
    script_overwrites(
        &worker,
        &[RoomHttpError::UnknownOutcome, RoomHttpError::AccessDenied],
    );
    press_approve(&mut worker, request);
    drain(&mut worker).await;
    assert_eq!(stored_grants(&worker), [OUTSIDER]);
    assert!(granted(&worker, OUTSIDER));
}

#[tokio::test]
async fn join_grant_witness_unknown_then_cancelled_retains_the_witness() {
    let (mut worker, _trace) = private_room().await;
    let request = requested(&mut worker).await;
    // Same shape, with the later definitive outcome a cancellation: still
    // no proof about the earlier ambiguous PUT.
    script_overwrites(
        &worker,
        &[RoomHttpError::UnknownOutcome, RoomHttpError::Cancelled],
    );
    press_approve(&mut worker, request);
    drain(&mut worker).await;
    assert_eq!(stored_grants(&worker), [OUTSIDER]);
    assert!(granted(&worker, OUTSIDER));
}

#[tokio::test]
async fn join_grant_witness_unknown_then_denied_survives_restart_and_public_revokes_only_connect() {
    let (mut worker, _trace) = private_room().await;
    // The member already holds unrelated bits on the room, and another
    // member carries a vote-kick Connect deny. Neither is the bot's grant
    // bit; revocation must preserve both exactly.
    let mut overwrites = worker.live_overwrites(ROOM).unwrap();
    overwrites.push(PermissionOverwrite {
        allow: Permissions::SPEAK,
        deny: Permissions::MUTE_MEMBERS,
        id: Id::new(OUTSIDER),
        kind: PermissionOverwriteType::Member,
    });
    overwrites.push(PermissionOverwrite {
        allow: Permissions::empty(),
        deny: Permissions::CONNECT,
        id: Id::new(VOTE_KICKED),
        kind: PermissionOverwriteType::Member,
    });
    let mut room_channel = channel(ROOM, 2, Some(CATEGORY));
    room_channel.permission_overwrites = Some(overwrites);
    worker.live.upsert_channel(room_channel);
    let request = requested(&mut worker).await;
    script_overwrites(
        &worker,
        &[RoomHttpError::UnknownOutcome, RoomHttpError::AccessDenied],
    );
    press_approve(&mut worker, request);
    drain(&mut worker).await;
    assert_eq!(stored_grants(&worker), [OUTSIDER]);
    // Restart hydration locates the retained witness, and `/public` can
    // therefore still take back exactly the bot-owned Connect bit.
    let mut after = restarted(&worker).await;
    assert!(granted(&after, OUTSIDER));
    let reply = after.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Public);
    assert!(reply.contains("public again"), "{reply}");
    drain(&mut after).await;
    let kept = member_overwrite(&after, OUTSIDER);
    assert!(!kept.allow.contains(Permissions::CONNECT));
    assert_eq!(kept.allow, Permissions::SPEAK);
    assert_eq!(kept.deny, Permissions::MUTE_MEMBERS);
    let kicked = member_overwrite(&after, VOTE_KICKED);
    assert_eq!(kicked.allow, Permissions::empty());
    assert_eq!(kicked.deny, Permissions::CONNECT);
    assert!(!granted(&after, OUTSIDER));
    assert!(stored_grants(&after).is_empty());
}

#[tokio::test]
async fn join_grant_witness_control_uncertain_revocation_retains_until_an_authoritative_retry() {
    let (mut worker, _trace) = private_room().await;
    let request = requested(&mut worker).await;
    press_approve(&mut worker, request);
    drain(&mut worker).await;
    assert_eq!(stored_grants(&worker), [OUTSIDER]);
    // `/public` lands its own write first; every revoke attempt after that
    // reports an unknown outcome, so the obligation cannot retire yet.
    let reply = worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Public);
    assert!(reply.contains("public again"), "{reply}");
    assert!(worker.dispatch_one(1_000_000).await);
    script_overwrites(&worker, &[RoomHttpError::UnknownOutcome; 40]);
    drain(&mut worker).await;
    assert_eq!(stored_grants(&worker), [OUTSIDER]);
    // A restart re-queues the revocation; with authoritative answers it
    // retires the witness and the Connect allow is gone.
    let mut after = restarted(&worker).await;
    drain(&mut after).await;
    assert!(stored_grants(&after).is_empty());
    assert!(
        !after
            .live_overwrites(ROOM)
            .unwrap()
            .iter()
            .any(|overwrite| overwrite.id.get() == OUTSIDER
                && overwrite.allow.contains(Permissions::CONNECT)),
        "the Connect allow is gone"
    );
}

#[tokio::test]
async fn join_grant_witness_control_fresh_first_refusal_unwinds_completely() {
    let (mut worker, _trace) = private_room().await;
    let request = requested(&mut worker).await;
    // Control: no earlier ambiguous attempt exists, so a definitive
    // first-attempt refusal proves the grant never landed and the fresh
    // intent unwinds: no witness, no granted claim, member may ask again.
    script_overwrites(&worker, &[RoomHttpError::AccessDenied]);
    press_approve(&mut worker, request);
    drain(&mut worker).await;
    assert!(stored_grants(&worker).is_empty());
    assert!(!granted(&worker, OUTSIDER));
    worker
        .live
        .voice_update_at(OUTSIDER, None, Some(false), 3_000);
    worker.reconcile();
    enter(&worker, OUTSIDER, 4_000);
    worker.reconcile();
    assert!(worker.privacy[&ROOM]
        .pending
        .contains_key(&MemberId(OUTSIDER)));
}

#[tokio::test]
async fn join_grant_witness_control_landed_grant_with_failed_move_stays_recorded() {
    let (mut worker, _trace) = private_room().await;
    let request = requested(&mut worker).await;
    // Control: the grant PUT lands and only the move fails, so both the
    // stored witness and the granted claim remain for `/public`.
    worker
        .http
        .move_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    press_approve(&mut worker, request);
    drain(&mut worker).await;
    assert_eq!(stored_grants(&worker), [OUTSIDER]);
    assert!(granted(&worker, OUTSIDER));
}
