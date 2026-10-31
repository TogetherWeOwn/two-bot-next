//! Acceptance tests: a guild voice actor whose first store load fails
//! transiently must recover on its own, with paced retries, without a
//! second guild snapshot, a restart, or a second live actor.
//!
//! The fake store's `rooms_errors` queue is the failure script: each
//! queued error fails exactly one `rooms()` call, so the number of
//! load attempts so far is `queued_before - queued_now`. `rooms()`
//! runs inside `GuildRoomWorker::load`, so consuming an error proves a
//! real initialization attempt happened. All timing uses the paused
//! tokio clock: the tests drive virtual time in one-second steps and
//! never assume a specific backoff schedule, only that retries are
//! paced (not a hot loop), each attempt is deadline-bounded, and
//! recovery completes well inside the driven window.

use super::*;

fn queue_rooms_errors(store: &Arc<Store>, count: usize, error: StoreError) {
    let mut errors = store.rooms_errors.lock().unwrap();
    for _ in 0..count {
        errors.push_back(error);
    }
}

fn queued_rooms_errors(store: &Arc<Store>) -> usize {
    store.rooms_errors.lock().unwrap().len()
}

async fn step_one_second() {
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
}

/// Drive virtual time until `condition` holds or `max_secs` of virtual
/// time pass. Returns whether the condition was reached.
async fn drive_until(max_secs: u64, mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..max_secs {
        if condition() {
            return true;
        }
        step_one_second().await;
    }
    condition()
}

async fn drive_secs(secs: u64) {
    for _ in 0..secs {
        step_one_second().await;
    }
}

/// An actor "answers" once its command loop is running, which only
/// happens after a successful load. While initialization is still
/// pending (or the actor died) the status probe stays unanswered.
async fn actor_answers(runtime: &Runtime) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_millis(50), runtime.worker_status(GUILD)).await,
        Ok(Some(_))
    )
}

async fn drive_until_actor_answers(runtime: &Runtime, max_secs: u64) -> bool {
    for _ in 0..max_secs {
        if actor_answers(runtime).await {
            return true;
        }
        step_one_second().await;
    }
    actor_answers(runtime).await
}

fn trace_has_move(trace: &Trace) -> bool {
    trace
        .lock()
        .unwrap()
        .iter()
        .any(|entry| entry.starts_with("move:"))
}

fn guild_delete_event() -> Event {
    Event::GuildDelete(
        serde_json::from_value::<twilight_model::gateway::payload::incoming::GuildDelete>(json!({
            "id": GUILD.to_string(), "unavailable": false
        }))
        .unwrap(),
    )
}

/// Core acceptance: one transient load failure, then the store works
/// again. Without any further snapshot the actor must come up and a
/// later creator join must reach the guarded create/move path.
#[tokio::test(start_paused = true)]
async fn actor_init_recovers_after_one_transient_load_failure_without_a_new_snapshot() {
    let (store, http, trace) = fixture();
    queue_rooms_errors(&store, 1, StoreError::Unavailable);
    let runtime = runtime(store.clone(), http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);

    // The scripted failure is really consumed: the first load attempt
    // happened and failed (otherwise recovery below proves nothing).
    assert!(
        drive_until(60, || queued_rooms_errors(&store) == 0).await,
        "the first load attempt must consume the scripted transient failure"
    );

    // No second GuildCreate, no RESUMED, no restart: virtual time and
    // ordinary yields alone must bring the actor up.
    assert!(
        drive_until_actor_answers(&runtime, 3_600).await,
        "actor must recover after a transient load failure without another snapshot"
    );
    assert_eq!(
        runtime.actors.lock().unwrap().len(),
        1,
        "recovery must not spawn a second live actor for the guild"
    );

    // A creator join after recovery reaches room creation and the move.
    feed(&runtime, &pipeline, voice_event(MEMBER, Some(CREATOR)));
    assert!(
        drive_until(600, || trace_has_move(&trace)).await,
        "a creator join after recovery must create and move, trace: {:?}",
        trace.lock().unwrap()
    );
}

/// Repeated transient failures must be retried with pacing: more than
/// one attempt over time, but nowhere near a hot loop, and once the
/// store works again the same single actor recovers.
#[tokio::test(start_paused = true)]
async fn actor_init_retries_are_paced_and_stay_a_single_actor() {
    let (store, http, _) = fixture();
    // Far more scripted failures than any paced retry policy could
    // consume in the driven window; a hot loop would drain them all.
    queue_rooms_errors(&store, 10_000, StoreError::Unavailable);
    let runtime = runtime(store.clone(), http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);

    // Ten virtual minutes of failing store.
    drive_secs(600).await;
    let attempts = 10_000 - queued_rooms_errors(&store);
    assert!(
        attempts >= 2,
        "initialization must retry after a transient failure, saw {attempts} attempt(s)"
    );
    assert!(
        attempts <= 600,
        "retries must be paced with capped backoff, not a hot loop: {attempts} attempts in 600s"
    );
    assert!(
        runtime.actors.lock().unwrap().len() <= 1,
        "retries must not accumulate multiple live actors for one guild"
    );

    // The store recovers: the pending initialization completes.
    store.rooms_errors.lock().unwrap().clear();
    assert!(
        drive_until_actor_answers(&runtime, 3_600).await,
        "actor must recover once the store stops failing"
    );
    assert_eq!(runtime.actors.lock().unwrap().len(), 1);
}

/// A load attempt that hangs must be abandoned at a finite deadline
/// and retried, not waited on forever. The first `rooms()` call
/// sleeps an hour of virtual time; recovery must arrive long before
/// that hour would elapse (the retry's own load succeeds at once).
#[tokio::test(start_paused = true)]
async fn actor_init_attempt_is_deadline_bounded_and_retried_after_a_hang() {
    let (store, http, _) = fixture();
    *store.rooms_delay.lock().unwrap() = Some(Duration::from_secs(3_600));
    let runtime = runtime(store.clone(), http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);

    assert!(
        drive_until_actor_answers(&runtime, 1_800).await,
        "a hung load attempt must hit its deadline and be retried within 30 virtual minutes"
    );
}

/// Guild removal while initialization retries are pending cancels
/// them: no further load attempts and no actor afterwards, even
/// though the store would succeed later.
#[tokio::test(start_paused = true)]
async fn actor_init_guild_removal_cancels_pending_retries_without_resurrection() {
    let (store, http, _) = fixture();
    queue_rooms_errors(&store, 10_000, StoreError::Unavailable);
    let runtime = runtime(store.clone(), http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);

    // Precondition: retries are genuinely in flight when removal lands.
    assert!(
        drive_until(3_600, || 10_000 - queued_rooms_errors(&store) >= 2).await,
        "removal test needs pending initialization retries to cancel"
    );
    feed(&runtime, &pipeline, guild_delete_event());
    let attempts_at_removal = 10_000 - queued_rooms_errors(&store);

    // Even with a healthy store afterwards, a removed guild stays gone.
    store.rooms_errors.lock().unwrap().clear();
    queue_rooms_errors(&store, 4, StoreError::Unavailable);
    drive_secs(1_800).await;
    assert_eq!(
        queued_rooms_errors(&store),
        4,
        "no load attempt may run for a removed guild (attempts at removal: {attempts_at_removal})"
    );
    assert!(
        runtime.live_actor(GUILD).is_none(),
        "a late initialization result must not resurrect a removed guild"
    );
}

/// A load already in flight when the guild is removed must not
/// publish an actor when it eventually succeeds.
#[tokio::test(start_paused = true)]
async fn actor_init_late_load_success_after_removal_publishes_nothing() {
    let (store, http, trace) = fixture();
    *store.rooms_delay.lock().unwrap() = Some(Duration::from_secs(120));
    let runtime = runtime(store.clone(), http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);
    drive_secs(5).await;

    feed(&runtime, &pipeline, guild_delete_event());
    // Let the in-flight load finish (and any would-be retry run).
    drive_secs(600).await;

    assert!(runtime.live_actor(GUILD).is_none());
    assert!(
        !actor_answers(&runtime).await,
        "no actor may answer for a removed guild"
    );
    assert!(
        trace.lock().unwrap().is_empty(),
        "a removed guild must see no Discord writes, trace: {:?}",
        trace.lock().unwrap()
    );
}

/// Disconnect during pending initialization holds every Discord
/// write back: whatever the retry does, nothing is created or moved
/// while the live state is invalidated.
#[tokio::test(start_paused = true)]
async fn actor_init_disconnect_during_pending_init_writes_nothing() {
    let (store, http, trace) = fixture();
    queue_rooms_errors(&store, 2, StoreError::Unavailable);
    let runtime = runtime(store.clone(), http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);
    assert!(
        drive_until(60, || queued_rooms_errors(&store) < 2).await,
        "the first load attempt must run before disconnect"
    );

    runtime.disconnect();
    feed(&runtime, &pipeline, voice_event(MEMBER, Some(CREATOR)));
    drive_secs(600).await;

    // Presence-span store writes are connectivity-independent (the
    // transition hook records them even while disconnected); only Discord
    // lifecycle writes are barred here.
    assert!(
        trace
            .lock()
            .unwrap()
            .iter()
            .all(|entry| entry.starts_with("evjoin:") || entry.starts_with("evleave:")),
        "no create/move/delete may be sent while disconnected, trace: {:?}",
        trace.lock().unwrap()
    );
    if let Some(actor) = runtime.live_actor(GUILD) {
        assert!(
            !actor.live.read_state().ready,
            "live state must stay not-ready after disconnect"
        );
    }
}

/// A credential refusal is not transient: initialization must not
/// retry it (no credential replacement or permission repair either;
/// the load is simply not re-attempted).
#[tokio::test(start_paused = true)]
async fn actor_init_credential_refusal_is_not_retried() {
    let (store, http, _) = fixture();
    queue_rooms_errors(&store, 8, StoreError::CredentialRefused);
    let runtime = runtime(store.clone(), http);
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);

    drive_secs(1_800).await;
    assert_eq!(
        queued_rooms_errors(&store),
        7,
        "exactly one load attempt may run after a credential refusal"
    );
    assert!(
        !actor_answers(&runtime).await,
        "no actor may come up on a refused credential"
    );
}

/// A disabled runtime never starts initialization at all.
#[tokio::test(start_paused = true)]
async fn actor_init_disabled_runtime_never_loads() {
    let (store, http, _) = fixture();
    queue_rooms_errors(&store, 3, StoreError::Unavailable);
    let store_for_runtime = store.clone();
    let runtime: Runtime = VoiceRuntime::new(
        move || (store_for_runtime.clone(), http.clone()),
        Duration::from_millis(10),
        false,
    );
    let pipeline = MemPipeline::for_replay();
    bootstrap(&runtime, &pipeline, &[]);
    drive_secs(600).await;

    assert_eq!(
        queued_rooms_errors(&store),
        3,
        "a disabled runtime must not attempt any load"
    );
    assert!(runtime.live_actor(GUILD).is_none());
}
