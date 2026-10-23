//! Protected test-only controls for saved state crossing both actor bounds.
//! The Builder imports this file verbatim. Reconcile churn may be shed;
//! acknowledged privacy/access/naming state and the startup owner seed may not.

#![cfg(test)]

use super::*;
use futures_util::poll;
use twilight_model::channel::permission_overwrite::PermissionOverwriteType;

#[derive(Clone, Copy, Debug)]
enum InitWindow {
    Attempt,
    Backoff,
    FailedAttempt,
}

const WINDOWS: [InitWindow; 3] = [
    InitWindow::Attempt,
    InitWindow::Backoff,
    InitWindow::FailedAttempt,
];

impl InitWindow {
    fn push(self, buffered: &mut VecDeque<ActorCommand>, command: ActorCommand) {
        match self {
            Self::Attempt | Self::FailedAttempt => buffer_attempt_command(buffered, command),
            Self::Backoff => buffer_init_command(buffered, command),
        }
    }

    fn overflow(self, buffered: &mut VecDeque<ActorCommand>) {
        for _ in 0..VOICE_ACTOR_INIT_BUFFER_CAP + 17 {
            self.push(buffered, ActorCommand::Reconcile);
        }
        assert!(
            buffered
                .iter()
                .filter(|command| matches!(command, ActorCommand::Reconcile))
                .count()
                <= VOICE_ACTOR_INIT_BUFFER_CAP,
            "{self:?}: ordinary FIFO must remain bounded"
        );
        if matches!(self, Self::FailedAttempt) {
            // Put a reply AFTER the flood so overflow cannot close it first.
            let (reply, mut response) = oneshot::channel();
            self.push(buffered, ActorCommand::Status(reply));
            assert!(matches!(
                response.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            purge_unanswerable_buffered(buffered);
            assert!(matches!(
                response.try_recv(),
                Err(oneshot::error::TryRecvError::Closed)
            ));
        }
    }
}

fn apply_buffer<S: RoomPersistence, H: RoomWrites>(
    worker: &mut GuildRoomWorker<S, H>,
    buffered: VecDeque<ActorCommand>,
) {
    let backpressure = ActorInboxBackpressure::default();
    for command in buffered {
        apply_command(worker, command, 0, &backpressure);
    }
}

fn assert_private_create<S: RoomPersistence>(worker: &GuildRoomWorker<S, Http>) {
    let created = worker.http.created_attributes.lock().unwrap();
    assert_eq!(created.len(), 1, "the next real room must be created");
    let everyone = created[0]
        .overwrites
        .iter()
        .find(|overwrite| {
            overwrite.id.get() == GUILD && overwrite.kind == PermissionOverwriteType::Role
        })
        .expect("a saved private default must emit an @everyone overwrite");
    assert!(
        everyone.deny.contains(Permissions::CONNECT),
        "a saved private default must deny Connect on the next actual room"
    );
    assert!(!everyone.allow.contains(Permissions::CONNECT));
}

#[tokio::test(flavor = "current_thread")]
async fn saved_alwaysprivate_reaches_a_loaded_worker_past_a_full_inbox() {
    // Break: creator_updated uses the gated sender after the store has saved.
    // The loaded worker must not escape through a cold creator read-through.
    let (live, store, http, trace) = fixture();
    live.publish(command_snapshot());
    let shared_creators = store.creators.clone();
    let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
    assert!(!worker.creators[&CREATOR].private_default);
    let make_creators = shared_creators.clone();
    let make_trace = trace.clone();
    let runtime = VoiceRuntime::new(
        move || {
            let mut store = Store::new(make_trace.clone());
            store.creators = make_creators.clone();
            (store, Http::new(make_trace.clone()))
        },
        Duration::from_millis(10),
        true,
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    runtime.actors.lock().unwrap().insert(
        GUILD,
        GuildActor {
            live: worker.live.clone(),
            tx,
            backpressure: ActorInboxBackpressure::default(),
        },
    );
    let actor = runtime
        .ensure_actor(GUILD)
        .expect("registered loaded worker");
    for _ in 0..VOICE_ACTOR_INBOX_CAP {
        assert!(actor.try_send_command(ActorCommand::Reconcile));
    }
    assert_eq!(actor.inbox_depth(), VOICE_ACTOR_INBOX_CAP);
    // No task owns rx, so even a yielding save cannot drain this inbox.
    let interaction = v8_interaction(
        "alwaysprivate",
        vec![
            typed_option("channel", CommandOptionValue::Channel(Id::new(CREATOR))),
            typed_option("enabled", CommandOptionValue::Boolean(true)),
        ],
    );
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned);
    assert!(
        shared_creators.lock().unwrap()[0].private_default,
        "durable save"
    );
    assert!(
        response_text(&response.expect("success reply")).contains("start private"),
        "the actual command must acknowledge the saved private setting"
    );
    while let Ok(command) = rx.try_recv() {
        actor.backpressure.release_for(&command);
        apply_command(&mut worker, command, 0, &actor.backpressure);
    }
    assert_eq!(actor.inbox_depth(), 0);
    assert_eq!(worker.store.creator_for_reads.load(Ordering::Relaxed), 0);
    join(&mut worker, MEMBER);
    dispatch(&mut worker, 0).await;
    assert_private_create(&worker);
    assert_eq!(worker.store.creator_for_reads.load(Ordering::Relaxed), 0);
    runtime.remove_guild(GUILD);
}

#[tokio::test]
async fn startup_owner_seed_survives_ordinary_init_overflow_and_failure_purge() {
    // Break: pop_front at the init cap silently discards the startup seed.
    for window in WINDOWS {
        let (live, store, http, trace) = fixture();
        store.creators.lock().unwrap()[0].name_template = "@@owner@@'s den".to_owned();
        store.rooms.lock().unwrap().insert(500, room(500));
        live.upsert_channel(channel(500, 2, Some(CATEGORY)));
        live.voice_update(MEMBER, Some(500), Some(false));
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        let mut buffered = VecDeque::new();
        window.push(
            &mut buffered,
            ActorCommand::DisplaySeed(vec![(MEMBER, "Alex".to_owned())]),
        );
        window.overflow(&mut buffered);
        apply_buffer(&mut worker, buffered);
        let mut expected = NameDirectory::default();
        expected.insert(MEMBER, "Alex".to_owned());
        assert_eq!(
            worker.name_directory, expected,
            "{window:?}: retain the actual owner name"
        );
        worker.refresh_template_names(0);
        dispatch(&mut worker, 0).await;
        assert_eq!(
            *trace.lock().unwrap(),
            ["rename:500:Alex's den"],
            "{window:?}"
        );
    }
}

#[tokio::test]
async fn latest_saved_naming_survives_ordinary_init_overflow_and_failure_purge() {
    for window in WINDOWS {
        let (live, store, http, _) = fixture();
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        let old = NameSettings::from_config(&empty_config());
        let new = NameSettings::from_config(&full_config());
        assert_ne!(old, new);
        worker.name_settings = old.clone();
        *worker.store.config.lock().unwrap() = full_config();
        let mut buffered = VecDeque::new();
        window.push(&mut buffered, ActorCommand::NameSettingsChanged(old));
        window.push(
            &mut buffered,
            ActorCommand::NameSettingsChanged(new.clone()),
        );
        window.overflow(&mut buffered);
        apply_buffer(&mut worker, buffered);
        assert_eq!(
            worker.name_settings, new,
            "{window:?}: latest saved naming state"
        );
        assert!(worker.name_settings_loaded);
    }
}

#[tokio::test]
async fn latest_saved_access_survives_ordinary_init_overflow_and_failure_purge() {
    for window in WINDOWS {
        let (live, store, http, _) = fixture();
        let old = AccessControls {
            room_creation_enabled: false,
            ..AccessControls::default()
        };
        let new = AccessControls {
            room_creation_enabled: true,
            ..AccessControls::default()
        };
        *store.access.lock().unwrap() = old.clone();
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        *worker.store.access.lock().unwrap() = new.clone();
        let mut buffered = VecDeque::new();
        window.push(&mut buffered, ActorCommand::AccessChanged(old));
        window.push(&mut buffered, ActorCommand::AccessChanged(new));
        window.overflow(&mut buffered);
        apply_buffer(&mut worker, buffered);
        let ticket = worker
            .live
            .voice_update(MEMBER, Some(CREATOR), Some(false))
            .unwrap();
        assert!(
            worker.accept_join(ticket, "new room", 7, NOW.to_owned()),
            "{window:?}: latest saved access must admit"
        );
        assert_eq!(worker.queue.pending_counts(GUILD), (1, 0));
    }
}

#[tokio::test]
async fn latest_saved_creator_privacy_survives_init_overflow_per_channel() {
    // Distinct creators must not overwrite each other during coalescing.
    for window in WINDOWS {
        let (live, store, http, _) = fixture();
        let other = CREATOR + 1;
        store
            .creators
            .lock()
            .unwrap()
            .push(CreatorChannel::new(GUILD, other));
        live.upsert_channel(channel(other, 2, Some(CATEGORY)));
        let mut worker = GuildRoomWorker::load(live, store, http).await.unwrap();
        let mut buffered = VecDeque::new();
        for (id, name) in [(CREATOR, "first saved"), (other, "second saved")] {
            let mut creator = worker.creators[&id].clone();
            assert!(!creator.private_default);
            window.push(&mut buffered, ActorCommand::CreatorAdded(creator.clone()));
            creator.private_default = true;
            creator.name_template = name.to_owned();
            worker.store.add_creator(&creator).await.unwrap();
            window.push(&mut buffered, ActorCommand::CreatorAdded(creator));
        }
        window.overflow(&mut buffered);
        apply_buffer(&mut worker, buffered);
        // Consume each actual creator policy without a reload or a store lookup.
        for (index, (id, name)) in [(CREATOR, "first saved"), (other, "second saved")]
            .into_iter()
            .enumerate()
        {
            assert!(
                worker.creators[&id].private_default,
                "{window:?}: channel {id}"
            );
            assert_eq!(
                worker.creators[&id].name_template, name,
                "{window:?}: channel {id}"
            );
            let ticket = worker
                .live
                .voice_update(MEMBER + index as u64, Some(id), Some(false))
                .unwrap();
            assert!(worker.accept_join(ticket, "new room", 7, NOW.to_owned()));
        }
        dispatch(&mut worker, 0).await;
        assert_private_create(&worker);
    }
}

#[test]
fn ordinary_init_fifo_is_bounded_and_preserves_nonoverflow_order() {
    for window in WINDOWS {
        let mut buffered = VecDeque::new();
        window.push(
            &mut buffered,
            ActorCommand::Display {
                member_id: MEMBER,
                display: "first".to_owned(),
            },
        );
        window.push(&mut buffered, ActorCommand::Reconcile);
        window.push(
            &mut buffered,
            ActorCommand::Display {
                member_id: MEMBER,
                display: "second".to_owned(),
            },
        );
        if matches!(window, InitWindow::FailedAttempt) {
            purge_unanswerable_buffered(&mut buffered);
        }
        assert_eq!(buffered.len(), 3, "{window:?}: no low-load loss");
        assert!(
            matches!(buffered.pop_front(), Some(ActorCommand::Display { display, .. }) if display == "first")
        );
        assert!(matches!(
            buffered.pop_front(),
            Some(ActorCommand::Reconcile)
        ));
        assert!(
            matches!(buffered.pop_front(), Some(ActorCommand::Display { display, .. }) if display == "second")
        );
        window.overflow(&mut buffered);
        assert!(
            buffered.len() <= VOICE_ACTOR_INIT_BUFFER_CAP,
            "{window:?}: ordinary-only FIFO bound"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn real_loader_keeps_startup_seed_after_receiving_more_than_init_capacity() {
    // Exercise receive -> release slot -> buffer -> load-success drain, not
    // just a helper: the 256-slot inbox must not hide the 512-slot init loss.
    let (live, store, _, trace) = fixture();
    *store.rooms_delay.lock().unwrap() = Some(Duration::from_secs(1));
    let store = Arc::new(store);
    let make = || (store.clone(), Http::new(trace.clone()));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let actor = GuildActor {
        live: live.clone(),
        tx,
        backpressure: ActorInboxBackpressure::default(),
    };
    let load = load_actor_worker(&make, live, &mut rx, &actor.backpressure);
    tokio::pin!(load);
    assert!(
        poll!(load.as_mut()).is_pending(),
        "load must stall on rooms read"
    );
    assert!(store.rooms_delay.lock().unwrap().is_none());
    assert!(
        actor.send_command_lossless(ActorCommand::DisplaySeed(vec![(MEMBER, "Alex".to_owned())]))
    );
    while actor.inbox_depth() != 0 {
        assert!(poll!(load.as_mut()).is_pending());
        tokio::task::yield_now().await;
    }
    let stalled_at = tokio::time::Instant::now();
    for _ in 0..VOICE_ACTOR_INIT_BUFFER_CAP + 17 {
        assert!(actor.try_send_command(ActorCommand::Reconcile));
        while actor.inbox_depth() != 0 {
            assert!(poll!(load.as_mut()).is_pending());
            // Reset Tokio's cooperative budget; a single poll is not a
            // guarantee that the next inbox entry was actually received.
            tokio::task::yield_now().await;
        }
        assert_eq!(actor.inbox_depth(), 0, "loader must release the gate slot");
    }
    assert_eq!(stalled_at.elapsed(), Duration::ZERO);
    tokio::time::advance(Duration::from_secs(1)).await;
    let (mut worker, buffered) = load.await.expect("successful stalled load");
    assert_eq!(
        actor.inbox_overflow_dropped(),
        0,
        "init overflow, not inbox overflow"
    );
    apply_buffer(&mut worker, buffered);
    let mut expected = NameDirectory::default();
    expected.insert(MEMBER, "Alex".to_owned());
    assert_eq!(
        worker.name_directory, expected,
        "real loader must retain the startup owner's actual name"
    );
}

#[tokio::test(start_paused = true)]
async fn real_loader_keeps_startup_seed_through_failure_and_backoff_overflow() {
    let (live, store, _, trace) = fixture();
    *store.rooms_delay.lock().unwrap() = Some(Duration::from_secs(1));
    store
        .rooms_errors
        .lock()
        .unwrap()
        .push_back(StoreError::Unavailable);
    let store = Arc::new(store);
    let make = || (store.clone(), Http::new(trace.clone()));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let actor = GuildActor {
        live: live.clone(),
        tx,
        backpressure: ActorInboxBackpressure::default(),
    };
    let load = load_actor_worker(&make, live, &mut rx, &actor.backpressure);
    tokio::pin!(load);
    assert!(poll!(load.as_mut()).is_pending());
    assert!(store.rooms_delay.lock().unwrap().is_none());
    assert!(
        actor.send_command_lossless(ActorCommand::DisplaySeed(vec![(MEMBER, "Alex".to_owned())]))
    );
    let (reply, mut response) = oneshot::channel();
    assert!(actor.send_command_lossless(ActorCommand::Status(reply)));
    assert!(poll!(load.as_mut()).is_pending());
    assert!(matches!(
        response.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(
        poll!(load.as_mut()).is_pending(),
        "failed attempt enters backoff"
    );
    assert!(
        store.rooms_errors.lock().unwrap().is_empty(),
        "scripted read failed"
    );
    assert!(
        matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ),
        "failure purge must promptly close a buffered reply"
    );
    let backoff_at = tokio::time::Instant::now();
    for _ in 0..VOICE_ACTOR_INIT_BUFFER_CAP + 17 {
        assert!(actor.try_send_command(ActorCommand::Reconcile));
        while actor.inbox_depth() != 0 {
            assert!(
                poll!(load.as_mut()).is_pending(),
                "must still be backing off"
            );
            tokio::task::yield_now().await;
        }
    }
    assert_eq!(backoff_at.elapsed(), Duration::ZERO);
    assert_eq!(actor.inbox_overflow_dropped(), 0);
    let (mut worker, buffered) = load.await.expect("successful retry after backoff");
    apply_buffer(&mut worker, buffered);
    let mut expected = NameDirectory::default();
    expected.insert(MEMBER, "Alex".to_owned());
    assert_eq!(
        worker.name_directory, expected,
        "failure purge plus backoff overflow must not lose the startup seed"
    );
}
