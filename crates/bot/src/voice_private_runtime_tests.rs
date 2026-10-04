//! V3 `/private` and `/public` against fake REST and a fake store: the room's
//! @everyone overwrite, the Join channel, persisted privacy and the block
//! list, restart adoption and the honest failure paths.

use super::*;
use two_bot_core::voice_private::{ChannelId, JoinChannel, MemberId};

const ROOM: u64 = 500;
const JOIN: u64 = 700;
const BOT: u64 = 999;
const OWNER: u64 = MEMBER;
const GUEST: u64 = 301;

type Worker = GuildRoomWorker<Store, Http>;

/// The room 500 fixture (owner `OWNER`, occupant `GUEST`), with fake channel
/// ids starting above the room so a created Join channel never collides.
fn parts() -> (LiveGuild, Store, Http, Trace) {
    let (live, store, http, trace) = owned_room();
    *http.next_id.lock().unwrap() = JOIN;
    (live, store, http, trace)
}

async fn worker() -> (Worker, Trace) {
    let (live, store, http, trace) = parts();
    (
        GuildRoomWorker::load(live, store, http).await.unwrap(),
        trace,
    )
}

/// Run every queued write to completion; each tick jumps past any backoff, so
/// a write that failed before the call is due again on the first tick.
async fn drain(worker: &mut Worker) {
    for step in 1..=40u64 {
        if !worker.dispatch_one(step * 1_000_000).await {
            return;
        }
    }
    panic!("queue did not drain");
}

fn private_cmd(worker: &mut Worker, actor: u64) -> String {
    worker.apply_privacy(actor, false, "Ana", PrivacyCommand::Private)
}

fn public_cmd(worker: &mut Worker, actor: u64) -> String {
    worker.apply_privacy(actor, false, "Ana", PrivacyCommand::Public)
}

fn calls(trace: &Trace) -> Vec<String> {
    trace.lock().unwrap().clone()
}

fn written(worker: &Worker) -> Vec<(u64, PermissionOverwrite)> {
    worker.http.written_overwrites.lock().unwrap().clone()
}

fn stored(worker: &Worker) -> Option<PrivacyRecord> {
    worker.store.privacy.lock().unwrap().get(&ROOM).cloned()
}

fn join_of(worker: &Worker) -> Option<JoinChannel> {
    worker
        .privacy
        .get(&ROOM)
        .and_then(|s| s.join_channel.clone())
}

fn voice_channel(id: u64, name: &str) -> Channel {
    let mut created = channel(id, 2, Some(CATEGORY));
    created.name = Some(name.to_owned());
    created
}

/// Replace the room's cached overwrites, as a hand edit would.
fn set_room_overwrites(live: &LiveGuild, overwrites: Vec<PermissionOverwrite>) {
    let mut room = channel(ROOM, 2, Some(CATEGORY));
    room.permission_overwrites = Some(overwrites);
    live.upsert_channel(room);
}

fn role_overwrite(id: u64, allow: Permissions, deny: Permissions) -> PermissionOverwrite {
    PermissionOverwrite {
        allow,
        deny,
        id: Id::new(id),
        kind: PermissionOverwriteType::Role,
    }
}

fn record(private: bool, join: Option<u64>, blocked: &[u64]) -> PrivacyRecord {
    PrivacyRecord {
        private,
        join_channel_id: join,
        blocked: blocked.iter().copied().collect(),
    }
}

#[tokio::test]
async fn private_denies_connect_keeps_view_and_creates_one_join_channel() {
    let (mut worker, trace) = worker().await;
    let reply = private_cmd(&mut worker, OWNER);
    assert!(reply.contains("Making this room private"), "{reply}");
    // The reply says what was queued: nothing changed until Discord accepts.
    assert!(!worker.privacy[&ROOM].private);
    assert!(calls(&trace).is_empty());
    drain(&mut worker).await;
    assert_eq!(
        calls(&trace),
        [
            // The bot keeps Connect it would lose with @everyone, first.
            format!("overwrite:{ROOM}:{BOT}:Member"),
            format!("overwrite:{ROOM}:{GUILD}:Role"),
            format!("save_privacy:{ROOM}:true:None:0"),
            format!("create_join:⇩ Join Ana:Some({CATEGORY}):None"),
            format!("save_privacy:{ROOM}:true:Some({JOIN}):0"),
        ]
    );
    let writes = written(&worker);
    let (_, bot) = &writes[0];
    assert_eq!(bot.allow, Permissions::CONNECT);
    assert!(bot.deny.is_empty());
    let (channel_id, everyone) = &writes[1];
    assert_eq!(*channel_id, ROOM);
    assert_eq!(everyone.deny, Permissions::CONNECT);
    // View Channel is on neither list, so the room stays visible.
    assert!(!everyone.allow.contains(Permissions::VIEW_CHANNEL));
    assert!(!everyone.deny.contains(Permissions::VIEW_CHANNEL));
    assert_eq!(
        join_of(&worker),
        Some(JoinChannel::Created {
            id: ChannelId(JOIN),
            name: "⇩ Join Ana".to_owned()
        })
    );
    assert_eq!(stored(&worker), Some(record(true, Some(JOIN), &[])));
    let live = worker.live.inner.read().unwrap();
    assert!(live.channels.contains_key(&JOIN));
    assert_eq!(live.channels[&JOIN].parent_id, Some(Id::new(CATEGORY)));
}

#[tokio::test]
async fn private_again_is_a_noop() {
    let (mut worker, trace) = worker().await;
    private_cmd(&mut worker, OWNER);
    drain(&mut worker).await;
    let before = calls(&trace);
    let again = private_cmd(&mut worker, OWNER);
    assert!(again.contains("already private"), "{again}");
    assert!(!worker.dispatch_one(99_000_000).await);
    assert_eq!(calls(&trace), before);
    assert_eq!(
        before
            .iter()
            .filter(|c| c.starts_with("create_join"))
            .count(),
        1
    );
}

#[tokio::test]
async fn private_twice_before_the_first_lands_still_makes_one_join_channel() {
    let (mut worker, trace) = worker().await;
    private_cmd(&mut worker, OWNER);
    private_cmd(&mut worker, OWNER);
    drain(&mut worker).await;
    let log = calls(&trace);
    assert_eq!(
        log.iter().filter(|c| c.starts_with("create_join")).count(),
        1
    );
    assert!(worker.privacy[&ROOM].private);
    assert_eq!(stored(&worker), Some(record(true, Some(JOIN), &[])));
}

#[tokio::test]
async fn public_restores_connect_deletes_the_join_channel_and_keeps_blocks() {
    let (mut worker, trace) = worker().await;
    private_cmd(&mut worker, OWNER);
    drain(&mut worker).await;
    // A block (written by the join-request slice) must outlive the toggle.
    worker
        .privacy
        .get_mut(&ROOM)
        .unwrap()
        .blocked
        .insert(MemberId(901));
    trace.lock().unwrap().clear();
    let reply = public_cmd(&mut worker, OWNER);
    assert!(reply.contains("public again"), "{reply}");
    drain(&mut worker).await;
    assert_eq!(
        calls(&trace),
        [
            format!("overwrite:{ROOM}:{GUILD}:Role"),
            format!("save_privacy:{ROOM}:false:None:1"),
            format!("delete:{JOIN}"),
        ]
    );
    let writes = written(&worker);
    let (_, everyone) = writes.last().unwrap();
    assert!(everyone.deny.is_empty());
    assert_eq!(stored(&worker), Some(record(false, None, &[901])));
    assert!(!worker
        .live
        .inner
        .read()
        .unwrap()
        .channels
        .contains_key(&JOIN));
    assert!(worker.join_deletable.is_empty());
    // Repeating is a no-op.
    let again = public_cmd(&mut worker, OWNER);
    assert!(again.contains("already public"), "{again}");
    assert!(!worker.dispatch_one(99_000_000).await);
    // Going private again keeps the same block list.
    private_cmd(&mut worker, OWNER);
    drain(&mut worker).await;
    assert_eq!(stored(&worker), Some(record(true, Some(JOIN + 1), &[901])));
}

#[tokio::test]
async fn deleting_the_room_deletes_its_join_channel_and_forgets_privacy() {
    let (mut worker, trace) = worker().await;
    private_cmd(&mut worker, OWNER);
    drain(&mut worker).await;
    trace.lock().unwrap().clear();
    worker.live.voice_update_at(OWNER, None, Some(false), 1_000);
    worker.live.voice_update_at(GUEST, None, Some(false), 1_001);
    worker.reconcile();
    drain(&mut worker).await;
    assert_eq!(
        calls(&trace),
        [
            format!("delete:{ROOM}"),
            format!("delete:{JOIN}"),
            format!("forget:{ROOM}")
        ]
    );
    assert!(worker.privacy.is_empty());
    assert!(worker.join_deletable.is_empty());
    assert!(worker.store.privacy.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_failed_join_channel_delete_retries_the_room_delete_instead_of_leaking_it() {
    let (mut worker, trace) = worker().await;
    private_cmd(&mut worker, OWNER);
    drain(&mut worker).await;
    trace.lock().unwrap().clear();
    // The room channel is already gone (hand-deleted), so its Discord delete is
    // skipped and the Join channel's delete is the first one to run.
    worker.live.remove_channel(ROOM);
    worker.live.voice_update_at(OWNER, None, Some(false), 1_000);
    worker.live.voice_update_at(GUEST, None, Some(false), 1_001);
    worker.reconcile();
    worker
        .http
        .delete_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::UnknownOutcome);
    assert!(worker.dispatch_one(0).await);
    assert_eq!(calls(&trace), [format!("delete:{JOIN}")]);
    assert!(
        worker.rooms.contains_key(&ROOM),
        "the row waits for the Join delete"
    );
    assert!(worker.store.rooms.lock().unwrap().contains_key(&ROOM));
    // The retry deletes the Join channel and only then forgets the room.
    drain(&mut worker).await;
    assert_eq!(
        calls(&trace),
        [
            format!("delete:{JOIN}"),
            format!("delete:{JOIN}"),
            format!("forget:{ROOM}")
        ]
    );
    assert!(worker.privacy.is_empty());
}

#[tokio::test]
async fn only_the_owner_or_an_admin_may_toggle_privacy() {
    let (mut worker, trace) = worker().await;
    let refused = private_cmd(&mut worker, GUEST);
    assert!(
        refused.contains("Only the room owner or an admin"),
        "{refused}"
    );
    let outside = private_cmd(&mut worker, 302);
    assert!(outside.contains("need to be in a voice room"), "{outside}");
    assert!(!worker.dispatch_one(0).await);
    assert!(calls(&trace).is_empty());
    assert!(worker.privacy.is_empty());
    // An admin in the room may; the Join channel keeps the stored owner name
    // (unknown, so unnamed) rather than the admin's.
    let admin = worker.apply_privacy(GUEST, true, "Admin", PrivacyCommand::Private);
    assert!(admin.contains("Making this room private"), "{admin}");
    drain(&mut worker).await;
    assert!(calls(&trace).contains(&format!("create_join:⇩ Join:Some({CATEGORY}):None")));
}

#[tokio::test]
async fn a_bot_without_manage_roles_refuses_and_changes_nothing() {
    let (live, store, http, trace) = parts();
    live.refresh_bot(BotAccess {
        member_id: BOT,
        guild_owner_id: 998,
        system_channel_id: None,
        member_roles: vec![],
        roles: vec![role(permissions() - Permissions::MANAGE_ROLES)],
    });
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    for command in [PrivacyCommand::Private, PrivacyCommand::Public] {
        let reply = worker.apply_privacy(OWNER, false, "Ana", command);
        // `/public` on a public room is a no-op; `/private` is refused.
        if command == PrivacyCommand::Private {
            assert!(reply.contains("Manage Roles"), "{reply}");
        }
    }
    assert!(!worker.dispatch_one(0).await);
    assert!(calls(&trace).is_empty());
    assert!(!worker.privacy[&ROOM].private);
}

#[tokio::test]
async fn a_refused_connect_write_changes_nothing_and_is_recorded() {
    let (mut worker, trace) = worker().await;
    worker
        .http
        .overwrite_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::AccessDenied);
    private_cmd(&mut worker, OWNER);
    drain(&mut worker).await;
    assert_eq!(calls(&trace), [format!("overwrite:{ROOM}:{BOT}:Member")]);
    assert!(!worker.privacy[&ROOM].private);
    assert_eq!(stored(&worker), None);
    assert!(worker.failures().contains(&LifecycleFailure::Discord {
        channel_id: ROOM,
        error: RoomHttpError::AccessDenied,
    }));
    // The room is still public, so `/private` can simply be run again.
    private_cmd(&mut worker, OWNER);
    drain(&mut worker).await;
    assert_eq!(stored(&worker), Some(record(true, Some(JOIN), &[])));
}

#[tokio::test]
async fn an_unknown_connect_outcome_dead_letters_without_flipping_the_room() {
    let (mut worker, _) = worker().await;
    worker.http.overwrite_errors.lock().unwrap().extend(
        std::iter::repeat_with(|| RoomHttpError::UnknownOutcome).take(QUEUE_MAX_ATTEMPTS as usize),
    );
    private_cmd(&mut worker, OWNER);
    for step in 0..u64::from(QUEUE_MAX_ATTEMPTS) {
        assert!(worker.dispatch_one(step * 1_000_000).await);
    }
    let failed = worker.queue.failed();
    assert_eq!(failed.len(), 1);
    assert_eq!(
        failed[0].action.action,
        RoomAction::SetEveryoneConnect {
            channel_id: ROOM,
            deny: true
        }
    );
    assert!(!worker.privacy[&ROOM].private);
    assert_eq!(stored(&worker), None);
}

#[tokio::test]
async fn a_failed_join_create_leaves_a_private_room_that_private_can_repair() {
    let (mut worker, trace) = worker().await;
    worker
        .http
        .join_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::Rejected {
            status: 400,
            code: 50035,
        });
    private_cmd(&mut worker, OWNER);
    drain(&mut worker).await;
    // Private (Discord accepted the deny), but no Join channel yet.
    assert!(worker.privacy[&ROOM].private);
    assert_eq!(join_of(&worker), None);
    assert_eq!(stored(&worker), Some(record(true, None, &[])));
    assert!(worker.failures().iter().any(|failure| matches!(
        failure,
        LifecycleFailure::Discord {
            error: RoomHttpError::Rejected { .. },
            ..
        }
    )));
    let reply = private_cmd(&mut worker, OWNER);
    assert!(reply.contains("Recreating its"), "{reply}");
    drain(&mut worker).await;
    assert_eq!(stored(&worker), Some(record(true, Some(JOIN), &[])));
    assert_eq!(
        calls(&trace)
            .iter()
            .filter(|c| c.starts_with("create_join"))
            .count(),
        2
    );
}

#[tokio::test]
async fn an_unknown_join_create_outcome_adopts_the_channel_instead_of_creating_another() {
    let (mut worker, trace) = worker().await;
    worker
        .http
        .join_errors
        .lock()
        .unwrap()
        .push_back(RoomHttpError::UnknownOutcome);
    private_cmd(&mut worker, OWNER);
    // Deny lands, then the create's outcome is unknown.
    assert!(worker.dispatch_one(0).await);
    assert!(worker.dispatch_one(1).await);
    // The POST had landed: the snapshot now shows the channel.
    worker
        .live
        .upsert_channel(voice_channel(JOIN, "⇩ Join Ana"));
    assert!(worker.dispatch_one(10_000_000).await);
    assert_eq!(
        calls(&trace)
            .iter()
            .filter(|c| c.starts_with("create_join"))
            .count(),
        1,
        "the retry adopts, it never POSTs again"
    );
    assert_eq!(stored(&worker), Some(record(true, Some(JOIN), &[])));
}

#[tokio::test]
async fn a_store_failure_retries_through_the_same_action_and_never_double_creates() {
    let (mut worker, trace) = worker().await;
    worker
        .store
        .save_privacy_errors
        .lock()
        .unwrap()
        .push_back(StoreError::Unavailable);
    private_cmd(&mut worker, OWNER);
    drain(&mut worker).await;
    // The first save failed, the retry (same action) wrote it; one Join POST.
    assert_eq!(stored(&worker), Some(record(true, Some(JOIN), &[])));
    assert_eq!(
        calls(&trace)
            .iter()
            .filter(|c| c.starts_with("create_join"))
            .count(),
        1
    );
}

#[tokio::test]
async fn emitted_overwrites_never_carry_manage_roles() {
    let (live, store, http, _) = parts();
    // A hand-edited room: @everyone explicitly allowed View, Connect and Manage
    // Roles.
    set_room_overwrites(
        &live,
        vec![role_overwrite(
            GUILD,
            Permissions::VIEW_CHANNEL | Permissions::CONNECT | Permissions::MANAGE_ROLES,
            Permissions::empty(),
        )],
    );
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    private_cmd(&mut worker, OWNER);
    drain(&mut worker).await;
    let writes = written(&worker);
    let (_, everyone) = writes.last().unwrap();
    // Connect moves to deny; View stays exactly as it was; Manage Roles is not
    // re-emitted.
    assert_eq!(everyone.allow, Permissions::VIEW_CHANNEL);
    assert_eq!(everyone.deny, Permissions::CONNECT);
    for (_, overwrite) in &writes {
        assert!(!overwrite.allow.contains(Permissions::MANAGE_ROLES));
    }
    public_cmd(&mut worker, OWNER);
    drain(&mut worker).await;
    for (_, overwrite) in written(&worker) {
        assert!(!overwrite.allow.contains(Permissions::MANAGE_ROLES));
    }
}

#[test]
fn the_everyone_overwrite_moves_only_connect() {
    let hidden = Permissions::VIEW_CHANNEL | Permissions::SPEAK;
    let current = [
        role_overwrite(GUILD, Permissions::CONNECT, hidden),
        role_overwrite(GUILD + 1, Permissions::CONNECT, Permissions::empty()),
    ];
    // Private: Connect leaves allow and joins deny; a hidden room stays hidden.
    let private = private_runtime::everyone_connect_overwrite(GUILD, &current, true);
    assert_eq!(private.id.get(), GUILD);
    assert_eq!(private.kind, PermissionOverwriteType::Role);
    assert_eq!(private.allow, Permissions::empty());
    assert_eq!(private.deny, hidden | Permissions::CONNECT);
    // Public: only the Connect deny is removed.
    let public = private_runtime::everyone_connect_overwrite(GUILD, &[private], false);
    assert_eq!(public.deny, hidden);
    assert_eq!(public.allow, Permissions::empty());
    // No existing entry: an empty overwrite plus the deny.
    let fresh = private_runtime::everyone_connect_overwrite(GUILD, &[], true);
    assert_eq!(fresh.allow, Permissions::empty());
    assert_eq!(fresh.deny, Permissions::CONNECT);
}

#[tokio::test]
async fn restart_adopts_a_join_channel_that_still_exists_and_deletes_it_on_public() {
    let (live, store, http, trace) = fixture();
    *http.next_id.lock().unwrap() = JOIN + 50;
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    store
        .privacy
        .lock()
        .unwrap()
        .insert(ROOM, record(true, Some(JOIN), &[901]));
    live.publish(snapshot(
        &[ROOM, JOIN],
        vec![VoiceMember {
            member_id: OWNER,
            channel_id: ROOM,
            bot: Some(false),
        }],
    ));
    // The room already denies Connect, as Discord holds it after `/private`.
    set_room_overwrites(
        &live,
        vec![role_overwrite(
            GUILD,
            Permissions::empty(),
            Permissions::CONNECT,
        )],
    );
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    assert!(worker.privacy[&ROOM].private);
    assert!(worker.join_deletable.contains(&JOIN));
    worker.reconcile();
    assert!(!worker.dispatch_one(0).await, "adoption queues nothing");
    assert_eq!(
        join_of(&worker),
        Some(JoinChannel::Created {
            id: ChannelId(JOIN),
            name: String::new()
        })
    );
    assert_eq!(worker.privacy[&ROOM].blocked.len(), 1);
    // Still private and unchanged: `/private` is a no-op after the restart.
    assert!(private_cmd(&mut worker, OWNER).contains("already private"));
    // `/public` deletes the adopted channel through REST.
    public_cmd(&mut worker, OWNER);
    drain(&mut worker).await;
    assert!(calls(&trace).contains(&format!("delete:{JOIN}")));
    assert_eq!(stored(&worker), Some(record(false, None, &[901])));
}

#[tokio::test]
async fn restart_forgets_a_join_channel_that_no_longer_exists() {
    let (live, store, http, trace) = fixture();
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    store
        .privacy
        .lock()
        .unwrap()
        .insert(ROOM, record(true, Some(JOIN), &[901]));
    // The snapshot has the room but no Join channel.
    live.publish(snapshot(
        &[ROOM],
        vec![VoiceMember {
            member_id: OWNER,
            channel_id: ROOM,
            bot: Some(false),
        }],
    ));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reconcile();
    assert_eq!(join_of(&worker), None);
    assert!(worker.privacy[&ROOM].private, "the room stays private");
    assert!(worker.join_deletable.is_empty());
    drain(&mut worker).await;
    assert_eq!(calls(&trace), [format!("save_privacy:{ROOM}:true:None:1")]);
    assert_eq!(stored(&worker), Some(record(true, None, &[901])));
    // Nothing was deleted for it: it was already gone.
    assert!(!calls(&trace).iter().any(|c| c.starts_with("delete")));
}

#[tokio::test]
async fn an_unreadable_snapshot_forgets_nothing() {
    let (live, store, http, _) = fixture();
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    store
        .privacy
        .lock()
        .unwrap()
        .insert(ROOM, record(true, Some(JOIN), &[]));
    live.publish(snapshot(&[ROOM], vec![]));
    live.disconnect();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reconcile();
    assert!(
        join_of(&worker).is_some(),
        "no authoritative snapshot, no forgetting"
    );
}

#[tokio::test]
async fn corrupt_and_orphan_records_are_not_loaded() {
    let (live, store, http, _) = fixture();
    store.rooms.lock().unwrap().insert(ROOM, room(ROOM));
    {
        let mut privacy = store.privacy.lock().unwrap();
        // A Join channel on a public room, and a record for an untracked room.
        privacy.insert(ROOM, record(false, Some(JOIN), &[]));
        privacy.insert(ROOM + 1, record(true, Some(JOIN + 1), &[]));
    }
    let worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    assert!(worker.privacy.is_empty());
    assert!(worker.join_deletable.is_empty());
}

#[tokio::test]
async fn a_join_channel_id_the_worker_does_not_hold_is_never_deleted() {
    let (mut worker, trace) = worker().await;
    worker
        .live
        .upsert_channel(voice_channel(JOIN + 9, "someone else's channel"));
    worker.queue.enqueue(
        GUILD,
        RoomAction::DeleteJoinChannel {
            room_channel_id: ROOM,
            channel_id: JOIN + 9,
        },
    );
    assert!(worker.dispatch_one(0).await);
    assert!(calls(&trace).is_empty());
    assert!(worker
        .live
        .inner
        .read()
        .unwrap()
        .channels
        .contains_key(&(JOIN + 9)));
}

#[test]
fn parse_private_and_public() {
    for (name, expected) in [
        ("private", VoiceCommand::Private),
        ("public", VoiceCommand::Public),
    ] {
        let interaction = voice_interaction(Some(command_data(name, Vec::new())), None, true);
        assert_eq!(expected.name(), name);
        assert_eq!(parse_voice_command(&interaction), Some(expected));
        assert!(two_bot_core::voice_access::VOICE_COMMANDS.contains(&name));
        let guildless = voice_interaction(Some(command_data(name, Vec::new())), None, false);
        assert_eq!(parse_voice_command(&guildless), None);
    }
}

#[tokio::test]
async fn handle_private_runs_through_the_actor_and_names_the_join_channel_after_the_caller() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    let interaction = voice_interaction_as(Some(command_data("private", Vec::new())), None, OWNER);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("Making this room private"), "{text}");
    wait_trace(&trace, &format!("overwrite:500:{GUILD}:Role")).await;
    wait_trace(
        &trace,
        &format!("create_join:⇩ Join member:Some({CATEGORY}):None"),
    )
    .await;
}

#[tokio::test]
async fn handle_private_refuses_a_non_owner_and_allows_an_admin() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    let interaction = voice_interaction_as(Some(command_data("private", Vec::new())), None, GUEST);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("Only the room owner or an admin"), "{text}");
    assert!(!calls(&trace).iter().any(|c| c.starts_with("overwrite")));
    let admin = voice_interaction_as(
        Some(command_data("private", Vec::new())),
        Some(Permissions::MANAGE_CHANNELS),
        GUEST,
    );
    let (_, response) = handle_capture(&runtime, &admin).await;
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("Making this room private"), "{text}");
    wait_trace(&trace, &format!("overwrite:500:{GUILD}:Role")).await;
}

#[tokio::test]
async fn handle_public_on_a_public_room_replies_and_writes_nothing() {
    let trace = Trace::default();
    let runtime = ownership_room_runtime(trace.clone()).await;
    let interaction = voice_interaction_as(Some(command_data("public", Vec::new())), None, OWNER);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    let text = response_text(response.as_ref().expect("reply"));
    assert!(text.contains("already public"), "{text}");
    assert!(!calls(&trace).iter().any(|c| c.starts_with("overwrite")));
}
