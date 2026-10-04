//! V3 join requests against fake REST and a fake store: the Join-channel entry,
//! the owner's prompt and its Approve / Deny / Block buttons, restart safety
//! and the honest failure paths.

use super::*;
use crate::voice_rooms::join_requests::{self, JoinClick};
use two_bot_core::{
    voice_custom_id::join_custom_id,
    voice_private::{JoinDecision, JoinRequest, MemberId},
};

const ROOM: u64 = 500;
const JOIN: u64 = 700;
const OWNER: u64 = MEMBER;
const GUEST: u64 = 301;
const OUTSIDER: u64 = 401;
const OTHER: u64 = 402;
const ADMIN: u64 = 777;

type Worker = GuildRoomWorker<Store, Http>;

/// The room 500 fixture (owner `OWNER`, occupant `GUEST`), with fake channel
/// ids starting above the room so a created Join channel never collides.
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

fn voice_channel(id: u64, name: &str) -> Channel {
    let mut created = channel(id, 2, Some(CATEGORY));
    created.name = Some(name.to_owned());
    created
}

/// A private room with its Join channel up and an empty trace.
async fn private_room() -> (Worker, Trace) {
    let (live, store, http, trace) = parts();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
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

fn press(worker: &mut Worker, actor: u64, decision: JoinDecision, request_id: u64) -> JoinReply {
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

fn button_ids(components: &[Component]) -> Vec<String> {
    let mut ids = Vec::new();
    for component in components {
        if let Component::ActionRow(row) = component {
            for inner in &row.components {
                if let Component::Button(button) = inner {
                    ids.push(button.custom_id.clone().expect("custom id"));
                }
            }
        }
    }
    ids
}

fn refused(reply: &JoinReply) -> &str {
    match reply {
        JoinReply::Refused(text) => text,
        JoinReply::Decided(text) => panic!("expected a refusal, got: {text}"),
    }
}

fn decided(reply: &JoinReply) -> &str {
    match reply {
        JoinReply::Decided(text) => text,
        JoinReply::Refused(text) => panic!("expected a decision, got: {text}"),
    }
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
    let http = Http::new(trace);
    *http.next_id.lock().unwrap() = JOIN + 50;
    GuildRoomWorker::load(worker.live.clone(), store, http)
        .await
        .unwrap()
}

#[tokio::test]
async fn entering_the_join_channel_asks_the_owner_once() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    // The request is pending at once; nothing is sent until the queue runs.
    let request = request_of(&worker, OUTSIDER);
    assert_eq!(request.owner_id, MemberId(OWNER));
    assert_ne!(request.id.0, 0);
    assert!(calls(&trace).is_empty());
    drain(&mut worker).await;
    let sent = prompts(&worker);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].channel, ROOM);
    // Only the owner is pinged; the requester is named by id, never by text.
    assert_eq!(sent[0].mention, Some(OWNER));
    assert!(sent[0].content.contains(&format!("<@{OWNER}>")));
    assert!(sent[0].content.contains(&format!("<@{OUTSIDER}>")));
    assert_eq!(
        button_ids(&sent[0].components),
        [
            join_requests::runtime_join_custom_id(
                JoinDecision::Approve,
                ROOM,
                request.id.0,
                worker.join.epoch,
            ),
            join_requests::runtime_join_custom_id(
                JoinDecision::Deny,
                ROOM,
                request.id.0,
                worker.join.epoch,
            ),
            join_requests::runtime_join_custom_id(
                JoinDecision::Block,
                ROOM,
                request.id.0,
                worker.join.epoch,
            ),
        ]
    );
    // The same stay, however many ticks, raises nothing more.
    worker.reconcile();
    worker.reconcile();
    drain(&mut worker).await;
    assert_eq!(prompts(&worker).len(), 1);
    assert_eq!(pending(&worker), [request]);
}

#[tokio::test]
async fn a_bot_and_a_blocked_member_raise_nothing() {
    let (mut worker, _trace) = private_room().await;
    worker
        .privacy
        .get_mut(&ROOM)
        .unwrap()
        .blocked
        .insert(MemberId(OUTSIDER));
    worker
        .live
        .voice_update_at(OTHER, Some(JOIN), Some(true), 2_000);
    enter(&worker, OUTSIDER, 2_001);
    worker.reconcile();
    drain(&mut worker).await;
    assert!(pending(&worker).is_empty());
    assert!(prompts(&worker).is_empty());
}

#[tokio::test]
async fn a_public_room_has_no_join_channel_to_enter() {
    let (live, store, http, _trace) = parts();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // A channel that happens to sit where a Join channel would, in a room that
    // never went private.
    worker
        .live
        .upsert_channel(voice_channel(JOIN, "⇩ Join Ana"));
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    assert!(worker.privacy.is_empty());
    assert!(worker.http.prompts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn only_the_owner_can_decide() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    trace.lock().unwrap().clear();
    let request = request_of(&worker, OUTSIDER);
    // An occupant and an admin are both refused: admin status is no input.
    for actor in [GUEST, ADMIN, OUTSIDER] {
        for decision in [
            JoinDecision::Approve,
            JoinDecision::Deny,
            JoinDecision::Block,
        ] {
            let reply = press(&mut worker, actor, decision, request.id.0);
            assert!(refused(&reply).contains("current owner"), "{reply:?}");
        }
    }
    drain(&mut worker).await;
    assert_eq!(pending(&worker), [request]);
    assert!(worker.privacy[&ROOM].granted.is_empty());
    assert!(worker.privacy[&ROOM].blocked.is_empty());
    assert!(calls(&trace).is_empty(), "{:?}", calls(&trace));
}

#[tokio::test]
async fn approve_grants_connect_on_the_room_only_and_moves_the_member_in() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    trace.lock().unwrap().clear();
    let request = request_of(&worker, OUTSIDER);
    let reply = press(&mut worker, OWNER, JoinDecision::Approve, request.id.0);
    assert!(decided(&reply).contains("Approved"), "{reply:?}");
    assert!(decided(&reply).contains(&format!("<@{OUTSIDER}>")));
    assert!(pending(&worker).is_empty());
    assert!(worker.privacy[&ROOM].granted.contains(&MemberId(OUTSIDER)));
    // Nothing is written until the queue runs, and then in this order.
    assert!(calls(&trace).is_empty());
    drain(&mut worker).await;
    assert_eq!(
        calls(&trace),
        [
            format!("overwrite:{ROOM}:{OUTSIDER}:Member"),
            format!("move:{OUTSIDER}:{ROOM}"),
        ]
    );
    let writes = worker.http.written_overwrites.lock().unwrap().clone();
    let (channel_id, grant) = writes.last().unwrap();
    assert_eq!(*channel_id, ROOM);
    assert_eq!(grant.id.get(), OUTSIDER);
    assert_eq!(grant.kind, PermissionOverwriteType::Member);
    // Connect on this member and this room only: nothing else is touched.
    assert_eq!(grant.allow, Permissions::CONNECT);
    assert!(grant.deny.is_empty());
    // The same button again is stale: answered, and nothing more is written.
    let again = press(&mut worker, OWNER, JoinDecision::Approve, request.id.0);
    assert!(refused(&again).contains("no longer pending"), "{again:?}");
    drain(&mut worker).await;
    assert_eq!(calls(&trace).len(), 2);
}

#[tokio::test]
async fn approve_does_not_move_a_member_who_left_the_join_channel() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    trace.lock().unwrap().clear();
    let request = request_of(&worker, OUTSIDER);
    press(&mut worker, OWNER, JoinDecision::Approve, request.id.0);
    // They wandered off before the queue ran: they keep the access they were
    // given, but are never pulled out of wherever they went.
    worker
        .live
        .voice_update_at(OUTSIDER, Some(CATEGORY), Some(false), 3_000);
    drain(&mut worker).await;
    assert_eq!(
        calls(&trace),
        [format!("overwrite:{ROOM}:{OUTSIDER}:Member")]
    );
}

#[tokio::test]
async fn approve_never_lifts_a_vote_kick_deny() {
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
    let reply = press(&mut worker, OWNER, JoinDecision::Approve, request.id.0);
    assert!(refused(&reply).contains("by a vote"), "{reply:?}");
    // Nothing changed: the request is still pending and can be denied.
    assert_eq!(pending(&worker), [request]);
    assert!(worker.privacy[&ROOM].granted.is_empty());
    let deny = press(&mut worker, OWNER, JoinDecision::Deny, request.id.0);
    assert!(decided(&deny).contains("Denied"), "{deny:?}");
    drain(&mut worker).await;
    assert!(calls(&trace).is_empty());
}

#[tokio::test]
async fn deny_grants_nothing_and_allows_a_fresh_request() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    trace.lock().unwrap().clear();
    let first = request_of(&worker, OUTSIDER);
    let reply = press(&mut worker, OWNER, JoinDecision::Deny, first.id.0);
    assert!(decided(&reply).contains("Denied"), "{reply:?}");
    drain(&mut worker).await;
    assert!(calls(&trace).is_empty());
    assert!(pending(&worker).is_empty());
    assert!(worker.privacy[&ROOM].granted.is_empty());
    assert!(worker.privacy[&ROOM].blocked.is_empty());
    // Still waiting in the Join channel is the same stay: no new request.
    worker.reconcile();
    assert!(pending(&worker).is_empty());
    // Leaving and coming back asks again, under a fresh id.
    leave(&worker, OUTSIDER, 3_000);
    worker.reconcile();
    enter(&worker, OUTSIDER, 4_000);
    worker.reconcile();
    drain(&mut worker).await;
    let second = request_of(&worker, OUTSIDER);
    assert!(second.id.0 > first.id.0);
    assert_eq!(prompts(&worker).len(), 2);
    // The first request's buttons stay dead.
    let stale = press(&mut worker, OWNER, JoinDecision::Approve, first.id.0);
    assert!(refused(&stale).contains("no longer pending"), "{stale:?}");
    assert_eq!(pending(&worker), [second]);
}

#[tokio::test]
async fn block_is_persisted_and_refuses_future_requests_even_after_a_restart() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    trace.lock().unwrap().clear();
    let request = request_of(&worker, OUTSIDER);
    let reply = press(&mut worker, OWNER, JoinDecision::Block, request.id.0);
    assert!(decided(&reply).contains("Blocked"), "{reply:?}");
    drain(&mut worker).await;
    assert_eq!(
        calls(&trace),
        [format!("save_privacy:{ROOM}:true:Some({JOIN}):1")]
    );
    assert_eq!(stored_blocks(&worker), [OUTSIDER]);
    // Coming back asks nobody.
    leave(&worker, OUTSIDER, 3_000);
    worker.reconcile();
    enter(&worker, OUTSIDER, 4_000);
    worker.reconcile();
    drain(&mut worker).await;
    assert!(pending(&worker).is_empty());
    assert_eq!(prompts(&worker).len(), 1);
    // The block is durable: a restarted worker still refuses silently.
    let mut after = restarted(&worker).await;
    leave(&after, OUTSIDER, 5_000);
    after.reconcile();
    enter(&after, OUTSIDER, 6_000);
    after.reconcile();
    drain(&mut after).await;
    assert!(after.privacy[&ROOM].blocked.contains(&MemberId(OUTSIDER)));
    assert!(pending(&after).is_empty());
    assert!(prompts(&after).is_empty());
}

#[tokio::test]
async fn an_ownership_transfer_withdraws_pending_requests_and_asks_the_new_owner() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    let old = request_of(&worker, OUTSIDER);
    let old_prompt = prompts(&worker)[0].message;
    trace.lock().unwrap().clear();
    let reply = worker.apply_ownership(
        OWNER,
        false,
        OwnershipCommand::Transfer { target_id: GUEST },
    );
    assert!(reply.contains("Transferred"), "{reply}");
    // The request raised to the previous owner is withdrawn at once.
    assert!(pending(&worker).is_empty());
    assert_eq!(worker.privacy[&ROOM].owner_id, MemberId(GUEST));
    // Its buttons answer nobody, whoever presses them.
    let by_new = press(&mut worker, GUEST, JoinDecision::Approve, old.id.0);
    assert!(refused(&by_new).contains("no longer pending"), "{by_new:?}");
    let by_old = press(&mut worker, OWNER, JoinDecision::Approve, old.id.0);
    assert!(refused(&by_old).contains("current owner"), "{by_old:?}");
    // The member is still waiting, so the new owner is asked, and the old
    // prompt loses its buttons.
    worker.reconcile();
    let fresh = request_of(&worker, OUTSIDER);
    assert_eq!(fresh.owner_id, MemberId(GUEST));
    assert!(fresh.id.0 > old.id.0);
    drain(&mut worker).await;
    let log = calls(&trace);
    assert!(
        log.contains(&format!(
            "edit_prompt:{}:{}:This join request is no longer pending.:0",
            old_prompt.channel_id, old_prompt.message_id
        )),
        "{log:?}"
    );
    let sent = prompts(&worker);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].mention, Some(GUEST));
    assert!(worker.join.prompts.contains_key(&(ROOM, fresh.id.0)));
    assert!(!worker.join.prompts.contains_key(&(ROOM, old.id.0)));
}

#[tokio::test]
async fn deleting_the_room_forgets_its_requests_and_their_prompts() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    assert_eq!(worker.join.prompts.len(), 1);
    trace.lock().unwrap().clear();
    // Everyone leaves: the room is deleted, and its privacy state goes with it.
    worker.live.voice_update_at(OWNER, None, Some(false), 3_000);
    worker.live.voice_update_at(GUEST, None, Some(false), 3_001);
    worker.reconcile();
    drain(&mut worker).await;
    assert!(worker.privacy.is_empty());
    assert!(worker.join.prompts.is_empty());
    assert!(worker.join.entries.is_empty());
    // The prompt lives in the room's chat and goes with the channel: no edit.
    assert!(!calls(&trace)
        .iter()
        .any(|call| call.starts_with("edit_prompt")));
}

#[tokio::test]
async fn public_takes_approved_access_back_and_retires_open_prompts() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    enter(&worker, OTHER, 2_001);
    worker.reconcile();
    drain(&mut worker).await;
    let approved = request_of(&worker, OUTSIDER);
    let open = request_of(&worker, OTHER);
    let open_prompt = worker
        .join
        .prompts
        .get(&(ROOM, open.id.0))
        .copied()
        .expect("prompt posted");
    press(&mut worker, OWNER, JoinDecision::Approve, approved.id.0);
    drain(&mut worker).await;
    assert!(worker.privacy[&ROOM].granted.contains(&MemberId(OUTSIDER)));
    trace.lock().unwrap().clear();
    let reply = worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Public);
    assert!(reply.contains("public again"), "{reply}");
    drain(&mut worker).await;
    let log = calls(&trace);
    // The approved member's own Connect allow goes (the room is open to all
    // again), and the request nobody answered loses its buttons.
    assert!(
        log.contains(&format!("delete_overwrite:{ROOM}:{OUTSIDER}")),
        "{log:?}"
    );
    assert!(
        log.contains(&format!(
            "edit_prompt:{}:{}:This join request is no longer pending.:0",
            open_prompt.channel_id, open_prompt.message_id
        )),
        "{log:?}"
    );
    assert!(!worker.privacy[&ROOM].private);
    assert!(worker.privacy[&ROOM].granted.is_empty());
    assert!(pending(&worker).is_empty());
    let live_overwrites = worker.live_overwrites(ROOM).unwrap();
    assert!(!live_overwrites
        .iter()
        .any(|overwrite| overwrite.id.get() == OUTSIDER));
    // A button from before `/public` is dead.
    let stale = press(&mut worker, OWNER, JoinDecision::Approve, open.id.0);
    assert!(refused(&stale).contains("no longer pending"), "{stale:?}");
}

#[tokio::test]
async fn a_button_from_before_a_restart_never_matches_a_request_raised_after() {
    let (mut worker, _trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    let before = request_of(&worker, OUTSIDER);
    // Restart: requests are runtime-only, so the worker forgets this one. The
    // member is still waiting, so the new worker asks the owner again.
    let mut after = restarted(&worker).await;
    assert_ne!(after.join.epoch, worker.join.epoch);
    after.reconcile();
    let fresh = request_of(&after, OUTSIDER);
    // The numeric counter restarted at the SAME id. Replay prevention must
    // come from the epoch, not a clock gap or process-global counter.
    assert_eq!(fresh.id, before.id);
    for decision in [
        JoinDecision::Approve,
        JoinDecision::Deny,
        JoinDecision::Block,
    ] {
        let old_id =
            join_requests::runtime_join_custom_id(decision, ROOM, before.id.0, worker.join.epoch);
        let click = join_component_action(&component_interaction(&old_id, None, OWNER))
            .expect("old button still routed");
        let stale = after.apply_join_decision(JoinDecisionCommand {
            actor_id: OWNER,
            click,
        });
        assert!(refused(&stale).contains("no longer pending"), "{stale:?}");
    }
    assert_eq!(pending(&after), [fresh]);
    assert!(after.privacy[&ROOM].granted.is_empty());
    // The new button works.
    let reply = press(&mut after, OWNER, JoinDecision::Approve, fresh.id.0);
    assert!(decided(&reply).contains("Approved"), "{reply:?}");
}

#[test]
fn worker_epochs_do_not_depend_on_wall_clock_or_a_process_global_counter() {
    let first = join_requests::JoinRequests::new();
    let second = join_requests::JoinRequests::new();
    assert_ne!(first.epoch, second.epoch);
}

#[tokio::test]
async fn legacy_buttons_without_a_worker_epoch_are_answered_with_a_refusal() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    let request = request_of(&worker, OUTSIDER);
    for decision in [
        JoinDecision::Approve,
        JoinDecision::Deny,
        JoinDecision::Block,
    ] {
        let legacy = join_custom_id(decision, ROOM, request.id.0);
        let click = join_component_action(&component_interaction(&legacy, None, OWNER))
            .expect("legacy button still routed");
        assert_eq!(click.epoch, None);
        let reply = worker.apply_join_decision(JoinDecisionCommand {
            actor_id: OWNER,
            click,
        });
        assert!(refused(&reply).contains("no longer pending"), "{reply:?}");
    }
    assert_eq!(pending(&worker), [request]);
    assert!(worker.privacy[&ROOM].granted.is_empty());
    assert!(worker.privacy[&ROOM].blocked.is_empty());
    assert!(calls(&trace).is_empty());
}

#[test]
fn runtime_button_ids_fit_discord_even_at_maximum_numeric_width() {
    for decision in [
        JoinDecision::Approve,
        JoinDecision::Deny,
        JoinDecision::Block,
    ] {
        let id = join_requests::runtime_join_custom_id(decision, u64::MAX, u64::MAX, [255; 16]);
        assert!(id.len() <= two_bot_core::voice_custom_id::MAX_VOICE_CUSTOM_ID_CHARS);
        let click = join_component_action(&component_interaction(&id, None, OWNER)).unwrap();
        assert_eq!(click.request_id, u64::MAX);
        assert_eq!(click.room_id, u64::MAX);
        assert_eq!(click.epoch, Some([255; 16]));
    }
}

#[tokio::test]
async fn a_room_chat_the_bot_cannot_post_in_drops_the_request_and_records_it() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    assert_eq!(prompts(&worker).len(), 1);
    // The chat refuses the next post. There is no DM fallback (a press on a DM
    // carries no guild), so nobody can be asked: the request is not left
    // pending behind buttons that were never delivered, and the failure is
    // recorded for `/setup`.
    worker
        .http
        .prompt_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    trace.lock().unwrap().clear();
    enter(&worker, OTHER, 3_000);
    worker.reconcile();
    drain(&mut worker).await;
    assert_eq!(calls(&trace), [format!("prompt:{ROOM}")]);
    assert!(!worker.privacy[&ROOM].pending.contains_key(&MemberId(OTHER)));
    assert!(worker.privacy[&ROOM]
        .pending
        .contains_key(&MemberId(OUTSIDER)));
    assert_eq!(prompts(&worker).len(), 1);
    assert!(!worker.failures().is_empty());
    // Coming back asks again, and the next post works.
    leave(&worker, OTHER, 4_000);
    worker.reconcile();
    enter(&worker, OTHER, 5_000);
    worker.reconcile();
    drain(&mut worker).await;
    assert_eq!(prompts(&worker).len(), 2);
}

#[tokio::test]
async fn a_request_answered_before_its_prompt_is_sent_posts_nothing() {
    let (mut worker, trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    // The owner answers through another path before the queue reaches the
    // prompt: the queued prompt must not resurrect the request.
    let request = request_of(&worker, OUTSIDER);
    let reply = press(&mut worker, OWNER, JoinDecision::Deny, request.id.0);
    assert!(decided(&reply).contains("Denied"), "{reply:?}");
    drain(&mut worker).await;
    assert!(prompts(&worker).is_empty());
    assert!(calls(&trace).is_empty());
}

#[tokio::test]
async fn a_failed_approval_write_is_not_claimed_so_the_member_can_ask_again() {
    let (mut worker, _trace) = private_room().await;
    enter(&worker, OUTSIDER, 2_000);
    worker.reconcile();
    drain(&mut worker).await;
    let request = request_of(&worker, OUTSIDER);
    worker
        .http
        .overwrite_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    let reply = press(&mut worker, OWNER, JoinDecision::Approve, request.id.0);
    assert!(decided(&reply).contains("Approved"), "{reply:?}");
    drain(&mut worker).await;
    // Discord refused the grant, so the state stops claiming it.
    assert!(worker.privacy[&ROOM].granted.is_empty());
    assert!(!worker.failures().is_empty());
    leave(&worker, OUTSIDER, 3_000);
    worker.reconcile();
    enter(&worker, OUTSIDER, 4_000);
    worker.reconcile();
    assert!(pending(&worker)
        .iter()
        .any(|request| request.member_id == MemberId(OUTSIDER)));
}

#[tokio::test]
async fn a_failed_move_keeps_the_grant_that_landed() {
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
    press(&mut worker, OWNER, JoinDecision::Approve, request.id.0);
    drain(&mut worker).await;
    // The member holds Connect on the room even though the move failed, so the
    // state keeps saying so and `/public` will take it back.
    assert_eq!(
        calls(&trace),
        [
            format!("overwrite:{ROOM}:{OUTSIDER}:Member"),
            format!("move:{OUTSIDER}:{ROOM}"),
        ]
    );
    assert!(worker.privacy[&ROOM].granted.contains(&MemberId(OUTSIDER)));
    assert!(!worker.failures().is_empty());
}

// --- the interaction surface --------------------------------------------------

fn join_id(decision: JoinDecision, room: u64, request: u64) -> String {
    join_requests::runtime_join_custom_id(decision, room, request, [1; 16])
}

#[test]
fn only_join_buttons_are_claimed_and_other_voice_ids_stay_silent() {
    for decision in [
        JoinDecision::Approve,
        JoinDecision::Deny,
        JoinDecision::Block,
    ] {
        let interaction = component_interaction(&join_id(decision, ROOM, 7), None, OWNER);
        assert_eq!(
            join_component_action(&interaction),
            Some(JoinClick {
                decision,
                room_id: ROOM,
                request_id: 7,
                epoch: Some([1; 16]),
            })
        );
    }
    for custom_id in [
        "two:voice:kick-yes:7000",
        "two:voice:name-custom:500",
        "two:voice:import-confirm:300:0123456789abcdef",
        "two:voice:join-approve:abc:1",
        "two:voice:join-approve:500",
        "two:voice:join-approve:500:1:extra",
        "two:voice:join-approve:500:1:0101010101010101010101010101010101:extra",
        "two:voice:join-approve:500:1:ABCDEF0123456789abcdef0123456789",
        "two:voice:join-approve:500:1:gggggggggggggggggggggggggggggggg",
        "two:voice:join-approve:0:1",
        "two:voice:join-approve:500:0",
        "two:voice:join-unknown:500:1",
        "two:lfg:join-approve:500:1",
        "votekick:7000:yes",
        "",
    ] {
        let interaction = component_interaction(custom_id, None, OWNER);
        assert_eq!(join_component_action(&interaction), None, "{custom_id}");
    }
    // Neither a guild-less press nor a slash command is a join button.
    assert_eq!(
        join_component_action(&guildless_component_interaction(&join_id(
            JoinDecision::Approve,
            ROOM,
            7
        ))),
        None
    );
    let slash = voice_interaction(Some(command_data("ping", Vec::new())), None, true);
    assert_eq!(join_component_action(&slash), None);
}

#[tokio::test]
async fn other_handlers_keep_unknown_voice_ids() {
    let runtime = gated_runtime(Trace::default(), AccessControls::default(), None);
    for custom_id in ["two:voice:kick-yes:7000", "two:voice:join-approve:abc:1"] {
        let interaction = component_interaction(custom_id, None, OWNER);
        let (owned, response) = handle_capture(&runtime, &interaction).await;
        assert!(!owned, "{custom_id}");
        assert!(response.is_none(), "{custom_id}");
    }
}

#[tokio::test]
async fn a_press_for_a_room_the_worker_does_not_track_is_refused_privately() {
    let runtime = gated_runtime(Trace::default(), AccessControls::default(), None);
    let interaction = component_interaction(&join_id(JoinDecision::Approve, ROOM, 7), None, OWNER);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let response = response.expect("a refusal");
    assert!(response_text(&response).contains("no longer exists"));
    assert_eq!(
        response.data.as_ref().and_then(|data| data.flags),
        Some(MessageFlags::EPHEMERAL)
    );
}

#[tokio::test]
async fn the_private_command_gate_applies_to_the_buttons() {
    let controls = AccessControls {
        command_roles: [("private".to_owned(), vec![7])].into(),
        ..AccessControls::default()
    };
    let runtime = gated_runtime(Trace::default(), controls, None);
    let click = || component_interaction(&join_id(JoinDecision::Deny, ROOM, 7), None, OWNER);
    let (owned, response) = handle_capture(&runtime, &click()).await;
    assert!(owned);
    assert_eq!(
        response_text(&response.expect("denial")),
        access_denied_text(AccessDenyReason::CommandRestricted)
    );
    // With the role the press reaches the worker (which has no such room).
    let (_, response) = handle_capture(&runtime, &with_roles(click(), &[7])).await;
    assert!(response_text(&response.expect("worker reply")).contains("no longer exists"));
}

#[tokio::test]
async fn the_responder_answers_a_decision_with_the_initial_callback_never_a_defer() {
    let trace = Trace::default();
    let runtime = gated_runtime(trace.clone(), AccessControls::default(), None);
    assert!(runtime.publish_snapshot(GUILD, command_snapshot()));
    let replies = Replies::new(trace.clone());
    let interaction = component_interaction(&join_id(JoinDecision::Deny, ROOM, 7), None, OWNER);
    VoiceResponder::respond_with(&runtime, &replies, &interaction, None, None).await;
    // The decision edits the prompt in place, which only the initial callback
    // can do.
    let log = calls(&trace);
    assert!(log.contains(&"respond".to_owned()), "{log:?}");
    assert!(!log.contains(&"defer".to_owned()), "{log:?}");
}

#[test]
fn a_decision_replaces_the_prompt_in_place_without_buttons_or_pings() {
    let response = join_requests::decided_response("Denied: <@401> was turned away.");
    assert_eq!(response.kind, InteractionResponseType::UpdateMessage);
    let data = response.data.expect("data");
    assert_eq!(data.components, Some(Vec::new()));
    assert_eq!(data.allowed_mentions, Some(AllowedMentions::default()));
    assert_eq!(data.flags, None);
}
