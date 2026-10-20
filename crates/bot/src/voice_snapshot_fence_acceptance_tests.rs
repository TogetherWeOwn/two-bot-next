//! Acceptance tests: fence voice privacy and join-request overwrites
//! against newer gateway state.
//!
//! `/private`, `/public` and join approve/revoke replace whole permission
//! overwrite entries over REST. Discord offers no atomic compare-and-swap
//! for that write, so the worker must fence it with the evidence it has:
//! the connection generation plus the exact cached overwrite snapshot it
//! planned from (the model the owner-grant writes already use). This file
//! pins the observable contract; how the builder threads the snapshot
//! through the private/join paths is their choice.
//!
//! Builder contract (this file is preserved verbatim; see the manifest on
//! the acceptance card):
//!
//! - Before each bot/@everyone privacy write and each approved-member
//!   grant/revoke write, recheck — at send time, through the write guard —
//!   that the gateway view is ready, still in the generation the write was
//!   planned in, the room channel still exists, the bot can still edit
//!   overwrites there, and the cached overwrites still equal the snapshot
//!   the planned whole-entry value was built from. A write planned from
//!   unknown evidence (`permission_overwrites` absent) must not be sent
//!   at all: absent evidence is not an empty entry.
//! - Stale evidence refuses or replans: the write is dropped (the queued
//!   action ends without claiming the state change) or it is replanned
//!   from the newer snapshot so every newer allow/deny bit survives. It
//!   never sends a whole-entry value built from the older snapshot.
//! - After an awaited PUT/DELETE succeeds, project the result into the
//!   cached snapshot only when the cache still holds the expected prior
//!   snapshot or already holds the matching own-write gateway echo. A
//!   concurrent gateway update landing during the await must not be
//!   replaced by the stale local projection, and the operation must not
//!   proceed to its next step (second overwrite, Join-channel creation
//!   or deletion, member move, flag flip, witness retirement) on that
//!   stale evidence.
//! - A matching own echo is success, not staleness: valid private/public
//!   toggles, approvals and revokes keep working when the gateway echoes
//!   each write back before the worker projects it.
//! - Unrelated allow/deny bits, vote-kick Connect denies and the bot's
//!   own room access survive every path. Ambiguous join-grant witnesses
//!   keep their existing acquisition/retirement semantics; a revoke that
//!   could not be planned from known evidence retires nothing.
//! - Document the fence and its limits where the privacy/join contracts
//!   live: it shrinks the race to the transport itself and never claims
//!   an upstream atomic CAS.
//!
//! The `fence_control_` tests use only today's seams and pass on the
//! baseline; the remaining `fence_` tests are the failing-first matrix.
//! Transport observations come from the fake REST trace and the written
//! overwrites; gateway changes are injected through `LiveGuild` exactly
//! as gateway frames would land them, including while a write is in
//! flight (the fake's `after_overwrite` hook runs after Discord accepted
//! the write and before the worker projects it).
//!
//! Skills used: test-driven-development.

use super::*;
use crate::voice_rooms::join_requests::JoinClick;
use std::sync::atomic::{AtomicBool, Ordering};
use two_bot_core::voice_private::{JoinDecision, MemberId};

const ROOM: u64 = 500;
const JOIN: u64 = 700;
const BOT: u64 = 999;
const OWNER: u64 = MEMBER;
const GUEST: u64 = 301;
const OUTSIDER: u64 = 401;

type Worker = GuildRoomWorker<Store, Http>;
/// One gateway-side change, applied to the live snapshot mid-flight.
type Mutation = Arc<dyn Fn(&LiveGuild) + Send + Sync>;

/// The room 500 fixture (owner `OWNER`, occupant `GUEST`), with fake
/// channel ids starting above the room so a created Join channel never
/// collides.
fn parts() -> (LiveGuild, Store, Http, Trace) {
    let (live, store, http, trace) = owned_room();
    *http.next_id.lock().unwrap() = JOIN;
    (live, store, http, trace)
}

async fn loaded(live: LiveGuild, store: Store, http: Http) -> Worker {
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // Some scenarios assert immediate lifecycle steps; see the private
    // runtime fixture of the same name.
    worker.live.set_empty_grace(Duration::ZERO);
    worker
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

fn written(worker: &Worker) -> Vec<(u64, PermissionOverwrite)> {
    worker.http.written_overwrites.lock().unwrap().clone()
}

fn written_for(
    worker: &Worker,
    kind: PermissionOverwriteType,
    id: u64,
) -> Vec<PermissionOverwrite> {
    written(worker)
        .into_iter()
        .map(|(_, overwrite)| overwrite)
        .filter(|overwrite| overwrite.kind == kind && overwrite.id.get() == id)
        .collect()
}

fn clear_observations(worker: &Worker, trace: &Trace) {
    trace.lock().unwrap().clear();
    worker.http.written_overwrites.lock().unwrap().clear();
}

fn is_private(worker: &Worker) -> bool {
    worker.privacy.get(&ROOM).is_some_and(|state| state.private)
}

fn stored_private(worker: &Worker) -> bool {
    worker
        .store
        .privacy
        .lock()
        .unwrap()
        .get(&ROOM)
        .is_some_and(|record| record.private)
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

/// The room's cached overwrite evidence: `None` when the channel is gone
/// or the gateway has not supplied overwrites for it.
fn cached_overwrites(worker: &Worker) -> Option<Vec<PermissionOverwrite>> {
    worker
        .live
        .inner
        .read()
        .unwrap()
        .channels
        .get(&ROOM)
        .and_then(|channel| channel.permission_overwrites.clone())
}

fn entry(
    overwrites: &[PermissionOverwrite],
    kind: PermissionOverwriteType,
    id: u64,
) -> Option<PermissionOverwrite> {
    overwrites
        .iter()
        .find(|overwrite| overwrite.kind == kind && overwrite.id.get() == id)
        .copied()
}

fn everyone_entry(worker: &Worker) -> PermissionOverwrite {
    let cached = cached_overwrites(worker).expect("room overwrites are known");
    entry(&cached, PermissionOverwriteType::Role, GUILD).expect("@everyone entry is cached")
}

fn member_entry(worker: &Worker, member: u64) -> Option<PermissionOverwrite> {
    let cached = cached_overwrites(worker).expect("room overwrites are known");
    entry(&cached, PermissionOverwriteType::Member, member)
}

fn role_overwrite(id: u64, allow: Permissions, deny: Permissions) -> PermissionOverwrite {
    PermissionOverwrite {
        allow,
        deny,
        id: Id::new(id),
        kind: PermissionOverwriteType::Role,
    }
}

fn member_overwrite(id: u64, allow: Permissions, deny: Permissions) -> PermissionOverwrite {
    PermissionOverwrite {
        allow,
        deny,
        id: Id::new(id),
        kind: PermissionOverwriteType::Member,
    }
}

/// Replace the room's cached overwrites wholesale, as a gateway
/// CHANNEL_UPDATE (or a fresh snapshot) would.
fn gateway_set_overwrites(live: &LiveGuild, overwrites: Option<Vec<PermissionOverwrite>>) {
    let mut room = channel(ROOM, 2, Some(CATEGORY));
    room.permission_overwrites = overwrites;
    live.upsert_channel(room);
}

/// Land one newer overwrite entry in the cache, keeping the rest, as a
/// concurrent gateway update for that target would.
fn gateway_replace_entry(live: &LiveGuild, replacement: PermissionOverwrite) {
    let mut room = live
        .read_state()
        .channels
        .get(&ROOM)
        .cloned()
        .expect("room channel is cached");
    let overwrites = room.permission_overwrites.get_or_insert_with(Vec::new);
    overwrites
        .retain(|existing| !(existing.kind == replacement.kind && existing.id == replacement.id));
    overwrites.push(replacement);
    live.upsert_channel(room);
}

/// Install a one-shot gateway change that lands while the next write for
/// `target` is in flight: the fake runs the hook after Discord accepted
/// that write, before the worker can project it or start its next step.
/// `put` selects the PUT (`true`) or the member DELETE (`false`) path.
/// The returned flag arms it, so fixture setup writes never trigger it.
fn install_mutation(
    http: &mut Http,
    live: &LiveGuild,
    target: u64,
    put: bool,
    mutation: Mutation,
) -> Arc<AtomicBool> {
    let armed = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&armed);
    let live = live.clone();
    http.after_overwrite = Some(Arc::new(move |channel_id, member_id, overwrite| {
        if channel_id == ROOM
            && member_id == target
            && overwrite.is_some() == put
            && flag.swap(false, Ordering::SeqCst)
        {
            mutation(&live);
        }
    }));
    armed
}

/// Install the gateway's own echo: every accepted write is mirrored
/// into the cache before the worker projects it.
fn install_echo(http: &mut Http, live: &LiveGuild) {
    let live = live.clone();
    http.after_overwrite = Some(Arc::new(move |channel_id, member_id, overwrite| {
        let mut echoed = live
            .read_state()
            .channels
            .get(&channel_id)
            .cloned()
            .expect("echoed channel exists");
        let overwrites = echoed.permission_overwrites.get_or_insert_with(Vec::new);
        match overwrite {
            Some(written) => {
                overwrites.retain(|existing| {
                    !(existing.kind == written.kind && existing.id == written.id)
                });
                overwrites.push(written);
            }
            None => overwrites.retain(|existing| {
                !(existing.kind == PermissionOverwriteType::Member
                    && existing.id.get() == member_id)
            }),
        }
        live.upsert_channel(echoed);
    }));
}

/// A private room with its Join channel up, an empty trace and no
/// recorded writes, ready for a join-request scenario.
async fn private_room() -> (Worker, Trace) {
    let (live, store, http, trace) = parts();
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    assert_eq!(worker.join_channel_of(ROOM), Some(JOIN));
    clear_observations(&worker, &trace);
    (worker, trace)
}

/// Raise one pending join request for `OUTSIDER` and return its id.
async fn pending_request(worker: &mut Worker) -> u64 {
    worker
        .live
        .voice_update_at(OUTSIDER, Some(JOIN), Some(false), 2_000);
    worker.reconcile();
    drain(worker).await;
    worker.privacy[&ROOM].pending[&MemberId(OUTSIDER)].id.0
}

fn press_approve(worker: &mut Worker, request_id: u64) {
    let reply = worker.apply_join_decision(JoinDecisionCommand {
        actor_id: OWNER,
        click: JoinClick {
            decision: JoinDecision::Approve,
            room_id: ROOM,
            request_id,
            epoch: Some(worker.join.epoch),
        },
    });
    assert!(matches!(reply, JoinReply::Decided(_)), "{reply:?}");
}

/// A private room where `OUTSIDER`'s approval has fully landed.
async fn approved_room() -> (Worker, Trace) {
    let (mut worker, trace) = private_room().await;
    let request = pending_request(&mut worker).await;
    press_approve(&mut worker, request);
    drain(&mut worker).await;
    assert!(worker.privacy[&ROOM].granted.contains(&MemberId(OUTSIDER)));
    assert_eq!(stored_grants(&worker), [OUTSIDER]);
    clear_observations(&worker, &trace);
    (worker, trace)
}

// --- controls: valid flows keep working ---------------------------------

#[tokio::test]
async fn fence_control_private_then_public_round_trip() {
    let (live, store, http, trace) = parts();
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    // Bot-member grant first, then @everyone — the order that can never
    // strand the bot between the two writes.
    let writes = written(&worker);
    assert_eq!(writes[0].1.id.get(), BOT);
    assert_eq!(writes[1].1.id.get(), GUILD);
    assert!(is_private(&worker) && stored_private(&worker));
    assert_eq!(worker.join_channel_of(ROOM), Some(JOIN));
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Public);
    drain(&mut worker).await;
    assert!(!is_private(&worker) && !stored_private(&worker));
    assert!(!everyone_entry(&worker).deny.contains(Permissions::CONNECT));
    assert!(calls(&trace).contains(&format!("delete:{JOIN}")));
}

#[tokio::test]
async fn fence_control_private_with_own_gateway_echoes() {
    let (live, store, mut http, _trace) = parts();
    install_echo(&mut http, &live);
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    assert!(is_private(&worker) && stored_private(&worker));
    assert_eq!(worker.join_channel_of(ROOM), Some(JOIN));
    assert!(everyone_entry(&worker).deny.contains(Permissions::CONNECT));
    let bot = member_entry(&worker, BOT).expect("bot entry is cached");
    assert!(bot.allow.contains(Permissions::CONNECT));
}

#[tokio::test]
async fn fence_control_approve_then_public_revoke_round_trip() {
    let (mut worker, trace) = approved_room().await;
    let grant = member_entry(&worker, OUTSIDER).expect("grant is cached");
    assert!(grant.allow.contains(Permissions::CONNECT));
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Public);
    drain(&mut worker).await;
    assert!(!is_private(&worker));
    assert_eq!(member_entry(&worker, OUTSIDER), None);
    assert!(stored_grants(&worker).is_empty());
    assert!(calls(&trace).contains(&format!("delete_overwrite:{ROOM}:{OUTSIDER}")));
}

#[tokio::test]
async fn fence_control_approve_with_own_gateway_echo() {
    let (live, store, mut http, trace) = parts();
    install_echo(&mut http, &live);
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    assert_eq!(worker.join_channel_of(ROOM), Some(JOIN));
    let request = pending_request(&mut worker).await;
    clear_observations(&worker, &trace);
    press_approve(&mut worker, request);
    drain(&mut worker).await;
    let grant = member_entry(&worker, OUTSIDER).expect("grant is cached");
    assert!(grant.allow.contains(Permissions::CONNECT));
    assert_eq!(stored_grants(&worker), [OUTSIDER]);
    assert!(calls(&trace).contains(&format!("move:{OUTSIDER}:{ROOM}")));
}

#[tokio::test]
async fn fence_control_queued_private_never_loses_newer_everyone_bits() {
    let (live, store, http, _trace) = parts();
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    // Queued but not yet dispatched: an admin edits @everyone meanwhile.
    gateway_set_overwrites(
        &worker.live.clone(),
        Some(vec![role_overwrite(
            GUILD,
            Permissions::VIEW_CHANNEL,
            Permissions::SPEAK,
        )]),
    );
    drain(&mut worker).await;
    // Refuse or replan are both honest; clobbering the newer bits in a
    // sent write or in the cache is not.
    for write in written_for(&worker, PermissionOverwriteType::Role, GUILD) {
        assert!(write.allow.contains(Permissions::VIEW_CHANNEL), "{write:?}");
        assert!(write.deny.contains(Permissions::SPEAK), "{write:?}");
    }
    let everyone = everyone_entry(&worker);
    assert!(everyone.allow.contains(Permissions::VIEW_CHANNEL));
    assert!(everyone.deny.contains(Permissions::SPEAK));
    if is_private(&worker) {
        assert!(everyone.deny.contains(Permissions::CONNECT));
    }
}

#[tokio::test]
async fn fence_control_queued_private_for_a_deleted_channel_writes_nothing() {
    let (live, store, http, trace) = parts();
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    worker.live.remove_channel(ROOM);
    drain(&mut worker).await;
    assert!(written(&worker).is_empty());
    assert!(!is_private(&worker) && !stored_private(&worker));
    assert!(!calls(&trace)
        .iter()
        .any(|call| call.starts_with("create_join")));
}

#[tokio::test]
async fn fence_control_queued_private_while_disconnected_writes_nothing() {
    let (live, store, http, _trace) = parts();
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    worker.live.disconnect();
    drain(&mut worker).await;
    assert!(written(&worker).is_empty());
    assert!(!is_private(&worker) && !stored_private(&worker));
}

#[tokio::test]
async fn fence_control_channel_deleted_between_bot_and_everyone_writes() {
    let (live, store, mut http, trace) = parts();
    let armed = install_mutation(
        &mut http,
        &live,
        BOT,
        true,
        Arc::new(|live| live.remove_channel(ROOM)),
    );
    armed.store(true, Ordering::SeqCst);
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    assert!(written_for(&worker, PermissionOverwriteType::Role, GUILD).is_empty());
    assert!(!is_private(&worker) && !stored_private(&worker));
    assert!(!calls(&trace)
        .iter()
        .any(|call| call.starts_with("create_join")));
}

#[tokio::test]
async fn fence_control_vote_deny_between_click_and_dispatch_blocks_the_grant() {
    let (mut worker, _trace) = private_room().await;
    let request = pending_request(&mut worker).await;
    press_approve(&mut worker, request);
    // A passed vote denies the member Connect before the queue runs.
    gateway_replace_entry(
        &worker.live.clone(),
        member_overwrite(OUTSIDER, Permissions::empty(), Permissions::CONNECT),
    );
    drain(&mut worker).await;
    assert!(written_for(&worker, PermissionOverwriteType::Member, OUTSIDER).is_empty());
    assert!(stored_grants(&worker).is_empty());
    let denied = member_entry(&worker, OUTSIDER).expect("vote deny is cached");
    assert!(denied.deny.contains(Permissions::CONNECT));
    assert!(!denied.allow.contains(Permissions::CONNECT));
}

#[tokio::test]
async fn fence_control_queued_approve_after_reconnect_never_loses_newer_member_bits() {
    let (mut worker, _trace) = private_room().await;
    let request = pending_request(&mut worker).await;
    press_approve(&mut worker, request);
    clear_observations(&worker, &_trace);
    // Reconnect before dispatch: the fresh snapshot shows the room still
    // private and the member holding unrelated bits.
    let live = worker.live.clone();
    live.disconnect();
    live.publish(snapshot(
        &[ROOM, JOIN],
        vec![
            VoiceMember {
                member_id: OWNER,
                channel_id: ROOM,
                bot: Some(false),
            },
            VoiceMember {
                member_id: GUEST,
                channel_id: ROOM,
                bot: Some(false),
            },
            VoiceMember {
                member_id: OUTSIDER,
                channel_id: JOIN,
                bot: Some(false),
            },
        ],
    ));
    gateway_set_overwrites(
        &live,
        Some(vec![
            role_overwrite(GUILD, Permissions::empty(), Permissions::CONNECT),
            member_overwrite(BOT, Permissions::CONNECT, Permissions::empty()),
            member_overwrite(OUTSIDER, Permissions::SPEAK, Permissions::MUTE_MEMBERS),
        ]),
    );
    drain(&mut worker).await;
    for write in written_for(&worker, PermissionOverwriteType::Member, OUTSIDER) {
        assert!(write.allow.contains(Permissions::SPEAK), "{write:?}");
        assert_eq!(write.deny, Permissions::MUTE_MEMBERS, "{write:?}");
    }
    let cached = member_entry(&worker, OUTSIDER).expect("member entry is cached");
    assert!(cached.allow.contains(Permissions::SPEAK));
    assert_eq!(cached.deny, Permissions::MUTE_MEMBERS);
}

// --- the fence: newer gateway state wins ---------------------------------

#[tokio::test]
async fn fence_private_everyone_step_never_loses_bits_landed_during_the_bot_write() {
    let (live, store, mut http, _trace) = parts();
    let armed = install_mutation(
        &mut http,
        &live,
        BOT,
        true,
        Arc::new(|live| {
            gateway_replace_entry(
                live,
                role_overwrite(GUILD, Permissions::VIEW_CHANNEL, Permissions::SPEAK),
            );
        }),
    );
    armed.store(true, Ordering::SeqCst);
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    // The @everyone value was planned before the admin's edit landed:
    // sending it (or projecting it) would erase the newer bits.
    let everyone = everyone_entry(&worker);
    assert!(everyone.allow.contains(Permissions::VIEW_CHANNEL));
    assert!(everyone.deny.contains(Permissions::SPEAK));
    if is_private(&worker) {
        assert!(stored_private(&worker));
        assert!(everyone.deny.contains(Permissions::CONNECT));
    } else {
        assert!(!stored_private(&worker));
    }
}

#[tokio::test]
async fn fence_private_bot_projection_never_replaces_a_newer_bot_entry() {
    let (live, store, mut http, _trace) = parts();
    let armed = install_mutation(
        &mut http,
        &live,
        BOT,
        true,
        Arc::new(|live| {
            gateway_replace_entry(
                live,
                member_overwrite(
                    BOT,
                    Permissions::CONNECT | Permissions::SPEAK,
                    Permissions::MUTE_MEMBERS,
                ),
            );
        }),
    );
    armed.store(true, Ordering::SeqCst);
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    // The accepted bot PUT alone may stand; its stale projection may not
    // erase the bits the gateway reported during the await.
    let bot = member_entry(&worker, BOT).expect("bot entry is cached");
    assert!(bot.allow.contains(Permissions::SPEAK));
    assert!(bot.deny.contains(Permissions::MUTE_MEMBERS));
    assert!(bot.allow.contains(Permissions::CONNECT));
}

#[tokio::test]
async fn fence_public_projection_never_replaces_newer_everyone_bits() {
    let (live, store, mut http, _trace) = parts();
    let armed = install_mutation(
        &mut http,
        &live,
        GUILD,
        true,
        Arc::new(|live| {
            gateway_replace_entry(
                live,
                role_overwrite(GUILD, Permissions::EMBED_LINKS, Permissions::SPEAK),
            );
        }),
    );
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    assert_eq!(worker.join_channel_of(ROOM), Some(JOIN));
    armed.store(true, Ordering::SeqCst);
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Public);
    drain(&mut worker).await;
    let everyone = everyone_entry(&worker);
    assert!(everyone.allow.contains(Permissions::EMBED_LINKS));
    assert!(everyone.deny.contains(Permissions::SPEAK));
    // The stored flag and the Join channel must agree: an aborted
    // `/public` keeps both, a completed one drops both.
    let join_present = worker
        .live
        .inner
        .read()
        .unwrap()
        .channels
        .contains_key(&JOIN);
    assert_eq!(stored_private(&worker), join_present);
    if !stored_private(&worker) {
        assert!(!everyone.deny.contains(Permissions::CONNECT));
    }
}

#[tokio::test]
async fn fence_private_everyone_step_dies_with_the_bot_permission() {
    let (live, store, mut http, trace) = parts();
    let armed = install_mutation(
        &mut http,
        &live,
        BOT,
        true,
        Arc::new(|live| {
            live.refresh_bot(BotAccess {
                member_id: BOT,
                guild_owner_id: 998,
                system_channel_id: None,
                member_roles: vec![],
                roles: vec![role(permissions() - Permissions::MANAGE_ROLES)],
            });
        }),
    );
    armed.store(true, Ordering::SeqCst);
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    // The bot-grant that already landed may stand; nothing may be sent
    // after the gateway reported the permission gone.
    assert!(written_for(&worker, PermissionOverwriteType::Role, GUILD).is_empty());
    assert!(!is_private(&worker) && !stored_private(&worker));
    assert!(!calls(&trace)
        .iter()
        .any(|call| call.starts_with("create_join")));
}

#[tokio::test]
async fn fence_private_everyone_step_never_loses_bits_from_a_reconnect_snapshot() {
    let (live, store, mut http, _trace) = parts();
    let armed = install_mutation(
        &mut http,
        &live,
        BOT,
        true,
        Arc::new(|live| {
            live.disconnect();
            live.publish(snapshot(
                &[ROOM],
                vec![
                    VoiceMember {
                        member_id: OWNER,
                        channel_id: ROOM,
                        bot: Some(false),
                    },
                    VoiceMember {
                        member_id: GUEST,
                        channel_id: ROOM,
                        bot: Some(false),
                    },
                ],
            ));
            gateway_set_overwrites(
                live,
                Some(vec![role_overwrite(
                    GUILD,
                    Permissions::EMBED_LINKS,
                    Permissions::empty(),
                )]),
            );
        }),
    );
    armed.store(true, Ordering::SeqCst);
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    // The plan predates the reconnect: only a refusal, or a replan from
    // the fresh snapshot, keeps the reconnect's @everyone bits.
    let everyone = everyone_entry(&worker);
    assert!(everyone.allow.contains(Permissions::EMBED_LINKS));
    if is_private(&worker) {
        assert!(stored_private(&worker));
        assert!(everyone.deny.contains(Permissions::CONNECT));
    } else {
        assert!(!stored_private(&worker));
    }
}

#[tokio::test]
async fn fence_private_with_unknown_overwrite_evidence_writes_nothing() {
    let (live, store, http, trace) = parts();
    let mut worker = loaded(live, store, http).await;
    // The gateway has not supplied this channel's overwrites: planning a
    // whole-entry replacement from an assumed-empty entry could erase
    // bits Discord actually holds.
    gateway_set_overwrites(&worker.live.clone(), None);
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    assert!(written(&worker).is_empty());
    assert!(!is_private(&worker) && !stored_private(&worker));
    assert!(!calls(&trace)
        .iter()
        .any(|call| call.starts_with("create_join")));
}

#[tokio::test]
async fn fence_approve_projection_never_replaces_newer_member_bits() {
    let (live, store, mut http, trace) = parts();
    let armed = install_mutation(
        &mut http,
        &live,
        OUTSIDER,
        true,
        Arc::new(|live| {
            gateway_replace_entry(
                live,
                member_overwrite(
                    OUTSIDER,
                    Permissions::CONNECT | Permissions::SPEAK,
                    Permissions::MUTE_MEMBERS,
                ),
            );
        }),
    );
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    let request = pending_request(&mut worker).await;
    press_approve(&mut worker, request);
    clear_observations(&worker, &trace);
    armed.store(true, Ordering::SeqCst);
    drain(&mut worker).await;
    let cached = member_entry(&worker, OUTSIDER).expect("member entry is cached");
    assert!(cached.allow.contains(Permissions::SPEAK));
    assert!(cached.deny.contains(Permissions::MUTE_MEMBERS));
    assert!(cached.allow.contains(Permissions::CONNECT));
}

#[tokio::test]
async fn fence_approve_landing_with_a_vote_deny_never_moves_or_masks_it() {
    let (live, store, mut http, trace) = parts();
    let armed = install_mutation(
        &mut http,
        &live,
        OUTSIDER,
        true,
        Arc::new(|live| {
            gateway_replace_entry(
                live,
                member_overwrite(OUTSIDER, Permissions::empty(), Permissions::CONNECT),
            );
        }),
    );
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    let request = pending_request(&mut worker).await;
    press_approve(&mut worker, request);
    clear_observations(&worker, &trace);
    armed.store(true, Ordering::SeqCst);
    drain(&mut worker).await;
    // A vote landed while the grant PUT was in flight: the cache must
    // keep showing the deny, and the member must not be moved in on the
    // strength of the stale plan.
    let cached = member_entry(&worker, OUTSIDER).expect("member entry is cached");
    assert!(cached.deny.contains(Permissions::CONNECT));
    assert!(!calls(&trace).contains(&format!("move:{OUTSIDER}:{ROOM}")));
}

#[tokio::test]
async fn fence_approve_with_unknown_overwrite_evidence_writes_nothing() {
    let (mut worker, trace) = private_room().await;
    let request = pending_request(&mut worker).await;
    press_approve(&mut worker, request);
    // Unknown evidence cannot exclude a vote-kick deny, so no grant may
    // be planned from it.
    gateway_set_overwrites(&worker.live.clone(), None);
    clear_observations(&worker, &trace);
    drain(&mut worker).await;
    assert!(written_for(&worker, PermissionOverwriteType::Member, OUTSIDER).is_empty());
    assert!(!calls(&trace).contains(&format!("move:{OUTSIDER}:{ROOM}")));
}

#[tokio::test]
async fn fence_revoke_projection_never_replaces_newer_member_bits() {
    let (live, store, mut http, trace) = parts();
    let armed = install_mutation(
        &mut http,
        &live,
        OUTSIDER,
        true,
        Arc::new(|live| {
            gateway_replace_entry(
                live,
                member_overwrite(
                    OUTSIDER,
                    Permissions::SPEAK | Permissions::EMBED_LINKS,
                    Permissions::MUTE_MEMBERS,
                ),
            );
        }),
    );
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    // Pre-existing unrelated bits make the revoke a kept-entry PUT.
    gateway_replace_entry(
        &worker.live.clone(),
        member_overwrite(OUTSIDER, Permissions::SPEAK, Permissions::MUTE_MEMBERS),
    );
    let request = pending_request(&mut worker).await;
    press_approve(&mut worker, request);
    drain(&mut worker).await;
    assert_eq!(stored_grants(&worker), [OUTSIDER]);
    clear_observations(&worker, &trace);
    armed.store(true, Ordering::SeqCst);
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Public);
    drain(&mut worker).await;
    let cached = member_entry(&worker, OUTSIDER).expect("member entry is cached");
    assert!(cached.allow.contains(Permissions::EMBED_LINKS));
    assert!(cached.allow.contains(Permissions::SPEAK));
    assert_eq!(cached.deny, Permissions::MUTE_MEMBERS);
    assert!(!cached.allow.contains(Permissions::CONNECT));
}

#[tokio::test]
async fn fence_revoke_delete_never_forgets_a_newer_member_entry() {
    let (live, store, mut http, trace) = parts();
    let armed = install_mutation(
        &mut http,
        &live,
        OUTSIDER,
        false,
        Arc::new(|live| {
            gateway_replace_entry(
                live,
                member_overwrite(OUTSIDER, Permissions::SPEAK, Permissions::empty()),
            );
        }),
    );
    let mut worker = loaded(live, store, http).await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Private);
    drain(&mut worker).await;
    let request = pending_request(&mut worker).await;
    press_approve(&mut worker, request);
    drain(&mut worker).await;
    assert_eq!(stored_grants(&worker), [OUTSIDER]);
    clear_observations(&worker, &trace);
    // The grant is Connect-only, so the revoke takes the DELETE path; an
    // admin re-creates the member's entry while it is in flight.
    armed.store(true, Ordering::SeqCst);
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Public);
    drain(&mut worker).await;
    let cached = member_entry(&worker, OUTSIDER).expect("the newer member entry must stay cached");
    assert!(cached.allow.contains(Permissions::SPEAK));
    assert!(!cached.allow.contains(Permissions::CONNECT));
}

#[tokio::test]
async fn fence_revoke_with_unknown_evidence_keeps_the_witness() {
    let (mut worker, trace) = approved_room().await;
    worker.apply_privacy(OWNER, false, "Ana", PrivacyCommand::Public);
    // Land `/public`'s own write first; the revoke is queued behind it.
    assert!(worker.dispatch_one(1_000_000).await);
    gateway_set_overwrites(&worker.live.clone(), None);
    clear_observations(&worker, &trace);
    drain(&mut worker).await;
    // Absent evidence is not proof the member entry is gone: nothing is
    // written and the durable witness survives for a later revoke.
    assert!(written_for(&worker, PermissionOverwriteType::Member, OUTSIDER).is_empty());
    assert!(!calls(&trace).contains(&format!("delete_overwrite:{ROOM}:{OUTSIDER}")));
    assert_eq!(stored_grants(&worker), [OUTSIDER]);
}
