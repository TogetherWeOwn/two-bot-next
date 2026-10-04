use super::*;

const LOBBY: u64 = 600;

#[tokio::test]
async fn boot_configured_infrastructure_is_protected_in_each_actor() {
    let values = HashMap::from([
        ("DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID", "600"),
        ("TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID", "201"),
        ("TWO_TEMP_VOICE_CATEGORY_ID", "401"),
        ("TWO_TEMP_VOICE_PROTECTED_CHANNEL_IDS", "700, 701"),
    ]);
    let protected =
        configured_protected_channels(|key| values.get(key).map(|s| s.to_string())).unwrap();
    assert_eq!(protected, HashSet::from([LOBBY, 201, 401, 700, 701]));
    let trace = Trace::default();
    let runtime = VoiceRuntime::new(
        move || (Store::new(trace.clone()), Http::new(trace.clone())),
        Duration::from_millis(250),
        true,
    )
    .with_protected_channels(protected);
    for guild in [GUILD, GUILD + 1] {
        let actor = runtime.ensure_actor(guild).unwrap();
        let state = actor.live.inner.read().unwrap();
        for id in [LOBBY, 201, 401, 700, 701] {
            assert!(state.delete_protected(id));
        }
    }
}

#[test]
fn malformed_protected_id_configuration_fails_closed() {
    assert!(configured_protected_channels(|_| None).unwrap().is_empty());
    for value in ["0", "01", "-1", "18446744073709551616", "bad", "1,"] {
        assert!(
            configured_protected_channels(|key| {
                (key == "TWO_TEMP_VOICE_PROTECTED_CHANNEL_IDS").then(|| value.to_owned())
            })
            .is_err(),
            "{value}"
        );
    }
    assert!(configured_protected_channels(|key| {
        (key == "DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID").then(|| "600,601".to_owned())
    })
    .is_err());
}

#[tokio::test(start_paused = true)]
async fn newly_added_creator_cancels_a_preexisting_delete() {
    let (live, store, http, trace) = fixture();
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    store.rooms.lock().unwrap().insert(500, room(500));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    apply_command(
        &mut worker,
        ActorCommand::CreatorAdded(CreatorChannel::new(GUILD, 500)),
        60_000,
    );
    dispatch(&mut worker, 60_000).await;
    assert!(trace.lock().unwrap().is_empty());
    assert!(worker.tracked().contains_key(&500));
}

#[tokio::test(start_paused = true)]
async fn companion_provenance_cannot_delete_infrastructure() {
    for id in [LOBBY, CREATOR, CATEGORY] {
        let (live, store, http, trace) = fixture();
        live.protect_channels([LOBBY]);
        live.upsert_channel(channel(LOBBY, 2, Some(CATEGORY)));
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        worker.companions.insert(
            500,
            TextCompanion {
                guild_id: GUILD,
                room_channel_id: 500,
                text_channel_id: id,
                settings: two_bot_core::voice_text_channel::TextChannelSettings {
                    enabled: true,
                    configured_name: None,
                    viewer_role_id: None,
                },
                created_at: NOW.to_owned(),
            },
        );
        assert!(!worker.delete_companion_for(500).await);
        assert!(trace.lock().unwrap().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn protected_infrastructure_with_claimed_provenance_never_deletes() {
    for id in [LOBBY, CREATOR, CATEGORY] {
        for compensate in [false, true] {
            let (live, store, http, trace) = fixture();
            live.protect_channels([LOBBY]);
            live.upsert_channel(channel(LOBBY, 2, Some(CATEGORY)));
            store.rooms.lock().unwrap().insert(id, room(id));
            let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
            tokio::time::advance(EMPTY_ROOM_GRACE).await;
            worker.reconcile();
            assert!(!worker.dispatch_one(60_000).await);
            // Also pin the dispatch guard, independently of reconcile's filter:
            // stale work and compensation must not override infrastructure IDs.
            worker.queue_delete(id, compensate);
            dispatch(&mut worker, 60_001).await;
            assert!(trace.lock().unwrap().is_empty(), "protected {id}");
            assert!(worker.tracked().contains_key(&id));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn queued_delete_observes_new_protection_at_send_time() {
    let (live, store, mut http, trace) = fixture();
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    store.rooms.lock().unwrap().insert(500, room(500));
    let shared = live.clone();
    http.before_delete = Some(Arc::new(move || shared.protect_channels([500])));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    dispatch(&mut worker, 60_000).await;
    assert!(trace.lock().unwrap().is_empty());
    assert!(worker.tracked().contains_key(&500));
}

#[tokio::test(start_paused = true)]
async fn ordinary_empty_room_survives_until_exact_grace_deadline() {
    let (live, store, http, trace) = fixture();
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    store.rooms.lock().unwrap().insert(500, room(500));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    // Direct stale work pins dispatch separately from the scheduling check.
    worker.queue_delete(500, false);
    dispatch(&mut worker, 0).await;
    assert!(trace.lock().unwrap().is_empty());
    worker.reconcile();
    assert!(!worker.dispatch_one(0).await);
    tokio::time::advance(EMPTY_ROOM_GRACE - Duration::from_millis(1)).await;
    worker.reconcile();
    assert!(!worker.dispatch_one(59_999).await);
    assert!(trace.lock().unwrap().is_empty());
    tokio::time::advance(Duration::from_millis(1)).await;
    worker.reconcile();
    dispatch(&mut worker, 60_000).await;
    assert_eq!(*trace.lock().unwrap(), ["delete:500", "forget:500"]);
}

#[tokio::test(start_paused = true)]
async fn human_join_and_leave_between_ticks_restart_queued_delete_grace() {
    for bot in [Some(false), None] {
        let (live, store, http, trace) = fixture();
        live.upsert_channel(channel(500, 2, Some(CATEGORY)));
        store.rooms.lock().unwrap().insert(500, room(500));
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        tokio::time::advance(EMPTY_ROOM_GRACE).await;
        worker.reconcile();
        // No intervening reconcile: the gateway publication itself cancels the
        // old empty marker and starts a fresh stay after the human leaves.
        worker.live.voice_update(MEMBER, Some(500), bot);
        worker.live.voice_update(MEMBER, None, bot);
        dispatch(&mut worker, 60_000).await;
        assert!(trace.lock().unwrap().is_empty());
        tokio::time::advance(EMPTY_ROOM_GRACE - Duration::from_millis(1)).await;
        worker.reconcile();
        assert!(!worker.dispatch_one(119_999).await);
        tokio::time::advance(Duration::from_millis(1)).await;
        worker.reconcile();
        dispatch(&mut worker, 120_000).await;
        assert_eq!(*trace.lock().unwrap(), ["delete:500", "forget:500"]);
    }
}

#[tokio::test(start_paused = true)]
async fn delayed_delete_rechecks_grace_after_transient_human_occupancy() {
    let (live, store, mut http, trace) = fixture();
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    store.rooms.lock().unwrap().insert(500, room(500));
    let shared = live.clone();
    http.before_delete = Some(Arc::new(move || {
        shared.voice_update(MEMBER, Some(500), Some(false));
        shared.voice_update(MEMBER, None, Some(false));
    }));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    dispatch(&mut worker, 60_000).await;
    assert!(trace.lock().unwrap().is_empty());
    assert!(worker.tracked().contains_key(&500));
}

#[tokio::test(start_paused = true)]
async fn reconnect_restarts_grace_instead_of_counting_disconnected_time() {
    let (live, store, http, trace) = fixture();
    live.upsert_channel(channel(500, 2, Some(CATEGORY)));
    store.rooms.lock().unwrap().insert(500, room(500));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    worker.live.disconnect();
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    assert!(!worker.dispatch_one(120_000).await);
    worker.live.publish(snapshot(&[500], vec![]));
    dispatch(&mut worker, 120_000).await; // stale pre-disconnect action cancels
    assert!(trace.lock().unwrap().is_empty());
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    dispatch(&mut worker, 180_000).await;
    assert_eq!(*trace.lock().unwrap(), ["delete:500", "forget:500"]);
}

#[tokio::test(start_paused = true)]
async fn bot_only_occupancy_is_empty_but_unknown_identity_is_human() {
    let (live, store, http, trace) = fixture();
    for id in [500, 501] {
        store.rooms.lock().unwrap().insert(id, room(id));
    }
    live.publish(snapshot(
        &[500, 501],
        vec![
            VoiceMember {
                member_id: 301,
                channel_id: 500,
                bot: Some(true),
            },
            VoiceMember {
                member_id: MEMBER,
                channel_id: 501,
                bot: None,
            },
        ],
    ));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    worker.reconcile();
    assert!(!worker.dispatch_one(0).await);
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.reconcile();
    // 501 may also persist its caretaker succession; drain both action kinds.
    for time in 60_000..60_004 {
        worker.dispatch_one(time).await;
    }
    let calls = trace.lock().unwrap();
    assert!(calls.contains(&"delete:500".to_owned()));
    assert!(!calls.contains(&"delete:501".to_owned()));
    assert!(worker.tracked().contains_key(&501));
}

#[tokio::test(start_paused = true)]
async fn direct_untracked_delete_cannot_trust_queued_provenance() {
    let (live, store, http, trace) = fixture();
    live.upsert_channel(channel(900, 2, Some(CATEGORY)));
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    tokio::time::advance(EMPTY_ROOM_GRACE).await;
    worker.queue_delete(900, false);
    dispatch(&mut worker, 60_000).await;
    assert!(trace.lock().unwrap().is_empty());
    assert!(worker
        .live
        .inner
        .read()
        .unwrap()
        .channels
        .contains_key(&900));
}
