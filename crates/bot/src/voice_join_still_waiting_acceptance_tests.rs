//! Acceptance tests: join decisions go only to members still waiting.
//!
//! Approving a join request tells the owner the member "is being let in and
//! moved" before the move lands, while the move silently no-ops when the
//! member left Join. Stale, answered and epoch-rolled presses all render the
//! same bare "no longer pending" with no next action for either side. Leaving
//! Join forgets the scan entry but never withdraws the pending request or
//! retires the prompt, so the owner keeps a live prompt for a gone member.
//!
//! Builder contract (this file is preserved verbatim; see the manifest on
//! the acceptance card). The outcomes below are the contract; the exact
//! copy and the press-time vs dispatch-time split are the Builder's choice,
//! as long as:
//!
//! - an approve pressed when the member is not in Join never claims
//!   movement; the owner learns what happened (left / no longer waiting /
//!   withdrawn) plus the recovery (ask them to rejoin);
//! - a refused move (Discord denies the move) is reported to the owner
//!   instead of staying a silent "moved" claim; either the press-time copy
//!   never claimed movement, or the dispatch corrects the owner with a
//!   prompt edit or follow-up prompt naming the failed move plus rejoin;
//! - a member who leaves after the press but before the dispatch is also
//!   corrected (no silent no-op move), with the grant kept and no pull out
//!   of wherever they went;
//! - terminal presses render per-side recovery copy: the requester (any
//!   non-owner) learns to leave and rejoin to ask again; the owner learns
//!   the request is gone and why (answered / withdrawn / restarted);
//! - leaving Join undecided withdraws the pending request and retires the
//!   prompt; rejoining raises a fresh request with a fresh id and prompt;
//! - deny, block and vote-kick-deny preservation hold exactly as today;
//! - restart/epoch invalidation stays a security property: no old button
//!   ever answers a new request, only clearer copy.
//!
//! No new grant authority, no prompt-expiry timers, no requester DMs, no
//! privacy-model changes. The epoch check stays before any state change.

use super::*;
use crate::voice_rooms::join_requests::{self, JoinClick};
use two_bot_core::voice_private::{JoinDecision, JoinRequest, MemberId};

const ROOM: u64 = 500;
const JOIN: u64 = 700;
const OWNER: u64 = MEMBER;
const OUTSIDER: u64 = 401;

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

fn calls(trace: &Trace) -> Vec<String> {
    trace.lock().unwrap().clone()
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

fn leave(worker: &Worker, member: u64, at: u64) {
    worker.live.voice_update_at(member, None, Some(false), at);
}

fn pending(worker: &Worker) -> Vec<JoinRequest> {
    worker.privacy[&ROOM].pending.values().copied().collect()
}

fn request_of(worker: &Worker, member: u64) -> JoinRequest {
    worker.privacy[&ROOM].pending[&MemberId(member)]
}

fn prompts(worker: &Worker) -> Vec<SentPrompt> {
    worker.http.prompts.lock().unwrap().clone()
}

fn press_as(worker: &mut Worker, actor: u64, decision: JoinDecision, request_id: u64) -> JoinReply {
    worker.apply_join_decision(JoinDecisionCommand {
        actor_id: actor,
        click: JoinClick {
            decision,
            room_id: ROOM,
            request_id,
            epoch: Some(worker.join.epoch),
        },
    })
}

fn press(worker: &mut Worker, decision: JoinDecision, request_id: u64) -> JoinReply {
    press_as(worker, OWNER, decision, request_id)
}

fn refused(reply: &JoinReply) -> &str {
    match reply {
        JoinReply::Refused(text) => text,
        JoinReply::Decided(text) => panic!("expected a refusal, got: {text}"),
    }
}

fn reply_text(reply: &JoinReply) -> &str {
    match reply {
        JoinReply::Refused(text) => text,
        JoinReply::Decided(text) => text,
    }
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

fn stored_blocks(worker: &Worker) -> Vec<u64> {
    worker
        .store
        .privacy
        .lock()
        .unwrap()
        .get(&ROOM)
        .map(|record| record.blocked.iter().copied().collect())
        .unwrap_or_default()
}

/// A worker for the same guild after a restart: same Discord, same stored
/// room and privacy record, nothing else remembered.
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
    for (room, members) in worker.store.join_grants.lock().unwrap().iter() {
        store
            .join_grants
            .lock()
            .unwrap()
            .insert(*room, members.clone());
    }
    let http = Http::new(trace);
    *http.next_id.lock().unwrap() = JOIN + 50;
    GuildRoomWorker::load(worker.live.clone(), store, http)
        .await
        .unwrap()
}

fn lower(text: &str) -> String {
    text.to_lowercase()
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

/// The requester recovery: leave and rejoin to ask again.
fn is_rejoin_copy(text_lower: &str) -> bool {
    text_lower.contains("leave")
        && (text_lower.contains("rejoin")
            || text_lower.contains("re-join")
            || text_lower.contains("join again"))
}

/// Any rejoin recovery wording: the owner copy needs only the rejoin part
/// ("ask them to rejoin"), not the requester's "leave" half.
fn has_rejoin(text_lower: &str) -> bool {
    contains_any(text_lower, &["rejoin", "re-join", "join again"])
}

/// The false movement claim the owner must never see for a gone member.
fn claims_movement(text_lower: &str) -> bool {
    text_lower.contains("being let in and moved") || text_lower.contains("moved to")
}

/// Every owner-visible prompt text: posted prompts plus queued edits.
fn owner_visible(worker: &Worker, trace: &Trace) -> String {
    let mut visible = calls(trace).join("\n");
    for prompt in prompts(worker) {
        visible.push('\n');
        visible.push_str(&prompt.content);
    }
    lower(&visible)
}

#[tokio::test]
async fn approve_after_leave_corrects_the_owner_without_claiming_movement() {
    let (mut worker, _trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    let request = request_of(&worker, OUTSIDER);
    // The member leaves before the owner answers.
    leave(&worker, OUTSIDER, 3_000);
    worker.reconcile();
    let reply = press(&mut worker, JoinDecision::Approve, request.id.0);
    let text = lower(reply_text(&reply));
    // RED on unfixed code: the press still claims "is being let in and
    // moved" with no rejoin recovery.
    assert!(
        !claims_movement(&text),
        "approve-after-leave must not claim movement, got: {text}"
    );
    assert!(
        contains_any(
            &text,
            &["left", "no longer", "withdrawn", "gone", "waiting"]
        ),
        "owner must learn what happened, got: {text}"
    );
    assert!(
        has_rejoin(&text),
        "owner must get the rejoin recovery, got: {text}"
    );
}

#[tokio::test]
async fn a_refused_move_is_reported_instead_of_a_silent_moved_claim() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    trace.lock().unwrap().clear();
    let request = request_of(&worker, OUTSIDER);
    worker
        .http
        .move_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let press_text = lower(reply_text(&press(
        &mut worker,
        JoinDecision::Approve,
        request.id.0,
    )));
    drain(&mut worker).await;
    // The grant landed, so it stays recorded for `/public`.
    assert!(worker.privacy[&ROOM].granted.contains(&MemberId(OUTSIDER)));
    assert_eq!(stored_grants(&worker), [OUTSIDER]);
    // The move was attempted and refused.
    assert!(
        calls(&trace)
            .iter()
            .any(|call| call == &format!("move:{OUTSIDER}:{ROOM}")),
        "move must be attempted, got: {:?}",
        calls(&trace)
    );
    // Either the press never claimed movement (press-time truth), or the
    // dispatch corrects the owner with the failed move plus rejoin.
    if claims_movement(&press_text) {
        let visible = owner_visible(&worker, &trace);
        assert!(
            contains_any(
                &visible,
                &[
                    "couldn't move",
                    "could not move",
                    "failed to move",
                    "move failed",
                    "could not be moved",
                ]
            ),
            "owner must learn the move failed after a moved claim, got press: {press_text} visible: {visible}"
        );
        assert!(
            has_rejoin(&visible),
            "owner must get the rejoin recovery, got: {visible}"
        );
    }
}

#[tokio::test]
async fn approve_then_leave_before_dispatch_corrects_the_owner() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    trace.lock().unwrap().clear();
    let request = request_of(&worker, OUTSIDER);
    let press_text = lower(reply_text(&press(
        &mut worker,
        JoinDecision::Approve,
        request.id.0,
    )));
    // They leave after the press but before the queue runs: the move must
    // no-op without pulling them out of wherever they went.
    worker
        .live
        .voice_update_at(OUTSIDER, Some(CATEGORY), Some(false), 3_000);
    drain(&mut worker).await;
    assert!(
        !calls(&trace).iter().any(|call| call.starts_with("move:")),
        "a gone member is never moved, got: {:?}",
        calls(&trace)
    );
    // They keep the access they were given.
    assert!(worker.privacy[&ROOM].granted.contains(&MemberId(OUTSIDER)));
    if claims_movement(&press_text) {
        let visible = owner_visible(&worker, &trace);
        assert!(
            contains_any(&visible, &["left", "no longer", "gone", "waiting"]),
            "owner must learn the member left after a moved claim, got press: {press_text} visible: {visible}"
        );
        assert!(
            has_rejoin(&visible),
            "owner must get the rejoin recovery, got: {visible}"
        );
    }
}

#[tokio::test]
async fn answered_presses_give_the_owner_actionable_copy() {
    for answer in [
        JoinDecision::Approve,
        JoinDecision::Deny,
        JoinDecision::Block,
    ] {
        let (mut worker, _trace) = private_room().await;
        enter(&worker, OUTSIDER, 2_000);
        worker.reconcile();
        drain(&mut worker).await;
        let request = request_of(&worker, OUTSIDER);
        let _ = press(&mut worker, answer, request.id.0);
        // The same button again is answered: the owner learns it is gone
        // and why, not the bare "no longer pending".
        let stale = press(&mut worker, JoinDecision::Approve, request.id.0);
        let text = lower(refused(&stale));
        assert!(
            contains_any(
                &text,
                &["answered", "decided", "denied", "approved", "blocked"]
            ),
            "owner must learn the request was answered ({answer:?}), got: {text}"
        );
        assert!(
            contains_any(&text, &["no longer", "gone", "pending"]),
            "owner must learn the request is gone ({answer:?}), got: {text}"
        );
    }
}

#[tokio::test]
async fn answered_presses_give_the_requester_rejoin_copy() {
    let (mut worker, _trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    let request = request_of(&worker, OUTSIDER);
    let _ = press(&mut worker, JoinDecision::Deny, request.id.0);
    // The requester pressing the answered button learns to leave and
    // rejoin to ask again, not the owner-only refusal.
    let stale = press_as(&mut worker, OUTSIDER, JoinDecision::Approve, request.id.0);
    let text = lower(refused(&stale));
    assert!(
        is_rejoin_copy(&text),
        "requester must learn to leave and rejoin, got: {text}"
    );
    assert!(
        !text.contains("current owner"),
        "requester stale copy must not be the owner-only refusal, got: {text}"
    );
}

#[tokio::test]
async fn epoch_rolled_presses_give_per_side_recovery_without_resurrection() {
    let (mut worker, _trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    let before = request_of(&worker, OUTSIDER);
    let mut after = restarted(&worker).await;
    assert_ne!(after.join.epoch, worker.join.epoch);
    after.reconcile();
    let fresh = request_of(&after, OUTSIDER);
    // The numeric counter restarted at the SAME id: replay prevention
    // comes from the epoch, and the copy must say the buttons restarted.
    assert_eq!(fresh.id, before.id);
    for decision in [
        JoinDecision::Approve,
        JoinDecision::Deny,
        JoinDecision::Block,
    ] {
        let old_id =
            join_requests::runtime_join_custom_id(decision, ROOM, before.id.0, worker.join.epoch);
        // The owner pressing the rolled button learns the buttons
        // restarted and the request is gone.
        let click = join_component_action(&component_interaction(&old_id, None, OWNER))
            .expect("old button still routed");
        let stale = after.apply_join_decision(JoinDecisionCommand {
            actor_id: OWNER,
            click,
        });
        let owner_text = lower(refused(&stale));
        assert!(
            contains_any(
                &owner_text,
                &["restart", "expired", "outdated", "new session"]
            ),
            "owner must learn the buttons restarted ({decision:?}), got: {owner_text}"
        );
        assert!(
            contains_any(&owner_text, &["no longer", "gone", "pending"]),
            "owner must learn the request is gone ({decision:?}), got: {owner_text}"
        );
        // The requester pressing the same rolled button learns to leave
        // and rejoin to ask again.
        let click = join_component_action(&component_interaction(&old_id, None, OUTSIDER))
            .expect("old button still routed");
        let stale = after.apply_join_decision(JoinDecisionCommand {
            actor_id: OUTSIDER,
            click,
        });
        let requester_text = lower(refused(&stale));
        assert!(
            is_rejoin_copy(&requester_text),
            "requester must learn to leave and rejoin ({decision:?}), got: {requester_text}"
        );
    }
    // No resurrection: the fresh request is still the only pending one.
    assert_eq!(pending(&after), [fresh]);
    assert!(after.privacy[&ROOM].granted.is_empty());
}

#[tokio::test]
async fn leaving_join_withdraws_and_retires_then_rejoin_reraises() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    let old = request_of(&worker, OUTSIDER);
    let old_prompt = worker
        .join
        .prompts
        .get(&(ROOM, old.id.0))
        .copied()
        .expect("prompt posted");
    // Leaving Join undecided withdraws the request at once and retires
    // its prompt.
    leave(&worker, OUTSIDER, 3_000);
    worker.reconcile();
    assert!(
        pending(&worker).is_empty(),
        "leave must withdraw the pending request"
    );
    assert!(
        !worker.join.prompts.contains_key(&(ROOM, old.id.0)),
        "leave must retire the prompt bookkeeping"
    );
    drain(&mut worker).await;
    let log = calls(&trace);
    assert!(
        log.iter().any(|call| call.starts_with(&format!(
            "edit_prompt:{}:{}:",
            old_prompt.channel_id, old_prompt.message_id
        ))),
        "leave must edit the prompt off, got: {log:?}"
    );
    // The withdrawn button answers per side: the owner learns it was
    // withdrawn, the requester learns to rejoin.
    let by_owner = press(&mut worker, JoinDecision::Approve, old.id.0);
    let owner_text = lower(refused(&by_owner));
    assert!(
        contains_any(
            &owner_text,
            &["withdrawn", "withdrew", "left", "no longer", "gone"]
        ),
        "owner must learn the request was withdrawn, got: {owner_text}"
    );
    let by_requester = press_as(&mut worker, OUTSIDER, JoinDecision::Approve, old.id.0);
    let requester_text = lower(refused(&by_requester));
    assert!(
        is_rejoin_copy(&requester_text),
        "requester must learn to leave and rejoin, got: {requester_text}"
    );
    // Coming back asks again, under a fresh id with a fresh prompt.
    enter(&worker, OUTSIDER, 4_000);
    worker.reconcile();
    drain(&mut worker).await;
    let fresh = request_of(&worker, OUTSIDER);
    assert!(fresh.id.0 > old.id.0);
    assert_eq!(fresh.owner_id, MemberId(OWNER));
    assert_eq!(prompts(&worker).len(), 2);
    assert!(worker.join.prompts.contains_key(&(ROOM, fresh.id.0)));
}

// --- controls: deny, block and vote-kick-deny preservation --------------------

#[tokio::test]
async fn control_deny_grants_nothing_and_rejoin_asks_again() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    trace.lock().unwrap().clear();
    let first = request_of(&worker, OUTSIDER);
    let reply = press(&mut worker, JoinDecision::Deny, first.id.0);
    assert!(reply_text(&reply).contains("Denied"), "{reply:?}");
    drain(&mut worker).await;
    assert!(calls(&trace).is_empty());
    assert!(pending(&worker).is_empty());
    assert!(worker.privacy[&ROOM].granted.is_empty());
    assert!(worker.privacy[&ROOM].blocked.is_empty());
    leave(&worker, OUTSIDER, 3_000);
    worker.reconcile();
    enter(&worker, OUTSIDER, 4_000);
    worker.reconcile();
    drain(&mut worker).await;
    let second = request_of(&worker, OUTSIDER);
    assert!(second.id.0 > first.id.0);
    assert_eq!(prompts(&worker).len(), 2);
}

#[tokio::test]
async fn control_block_is_persisted_and_refuses_future_requests() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    trace.lock().unwrap().clear();
    let request = request_of(&worker, OUTSIDER);
    let reply = press(&mut worker, JoinDecision::Block, request.id.0);
    assert!(reply_text(&reply).contains("Blocked"), "{reply:?}");
    drain(&mut worker).await;
    assert_eq!(stored_blocks(&worker), [OUTSIDER]);
    leave(&worker, OUTSIDER, 3_000);
    worker.reconcile();
    enter(&worker, OUTSIDER, 4_000);
    worker.reconcile();
    drain(&mut worker).await;
    assert!(pending(&worker).is_empty());
    assert_eq!(prompts(&worker).len(), 1);
}

#[tokio::test]
async fn control_approve_still_refuses_a_vote_kick_deny() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    trace.lock().unwrap().clear();
    let request = request_of(&worker, OUTSIDER);
    // A passed vote left a member-scoped Connect deny on the room.
    let mut overwrites = worker.live_overwrites(ROOM).unwrap();
    overwrites.push(PermissionOverwrite {
        allow: Permissions::empty(),
        deny: Permissions::CONNECT,
        id: Id::new(OUTSIDER),
        kind: PermissionOverwriteType::Member,
    });
    let mut channel = channel(ROOM, 2, Some(CATEGORY));
    channel.permission_overwrites = Some(overwrites);
    worker.live.upsert_channel(channel);
    let reply = press(&mut worker, JoinDecision::Approve, request.id.0);
    assert!(refused(&reply).contains("by a vote"), "{reply:?}");
    assert_eq!(pending(&worker), [request]);
    assert!(worker.privacy[&ROOM].granted.is_empty());
    let deny = press(&mut worker, JoinDecision::Deny, request.id.0);
    assert!(reply_text(&deny).contains("Denied"), "{deny:?}");
    drain(&mut worker).await;
    assert!(calls(&trace).is_empty());
}
