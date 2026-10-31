//! Runtime adoption, not another pure-helper range test: reverting the
//! load_actor_worker wait to voice_actor_load_backoff must fail below.
//! The existing schedule-agnostic init_recovery suite stays unchanged.

#![cfg(test)]

use super::*;
use futures_util::poll;
use tokio::time::{advance, Instant};

// Choose two guild seeds with clearly separated first-failure offsets. The
// pure helper already has independent range/determinism acceptance; here its
// contract is that the REAL initializer uses that guild's selected wait.
fn separated_guilds() -> [(u64, Duration); 2] {
    let mut candidates: Vec<_> = (81_000..81_064)
        .map(|guild| (guild, voice_actor_load_backoff_jittered(1, guild)))
        .filter(|(_, wait)| {
            *wait >= Duration::from_millis(20) && *wait < Duration::from_millis(950)
        })
        .collect();
    candidates.sort_by_key(|(_, wait)| *wait);
    let early = *candidates.first().expect("fixture needs guild draws");
    let late = *candidates.last().unwrap();
    assert!(
        late.1 - early.1 > Duration::from_millis(50),
        "fixture needs distinct, timer-resolvable guild offsets"
    );
    [early, late]
}

fn load_fixture(
    guild: u64,
    completed_reads: Arc<Mutex<Vec<Instant>>>,
) -> (LiveGuild, Arc<Store>, Trace) {
    let (live, mut store, _, trace) = super::super::fixture_for_guild(guild);
    store
        .rooms_errors
        .lock()
        .unwrap()
        .push_back(StoreError::Unavailable);
    store.after_rooms = Some(Arc::new(move || {
        completed_reads.lock().unwrap().push(Instant::now());
    }));
    (live, Arc::new(store), trace)
}

#[tokio::test(start_paused = true)]
async fn actor_init_second_persistence_load_uses_each_guilds_jittered_offset() {
    let [(early_guild, early_wait), (late_guild, late_wait)] = separated_guilds();
    let early_reads = Arc::new(Mutex::new(Vec::new()));
    let late_reads = Arc::new(Mutex::new(Vec::new()));
    let (early_live, early_store, early_trace) = load_fixture(early_guild, early_reads.clone());
    let (late_live, late_store, late_trace) = load_fixture(late_guild, late_reads.clone());
    let make_early = || (early_store.clone(), Http::new(early_trace.clone()));
    let make_late = || (late_store.clone(), Http::new(late_trace.clone()));
    // Keep senders alive: a closed actor inbox is cancellation, not a retry.
    let (_early_tx, mut early_rx) = mpsc::unbounded_channel();
    let (_late_tx, mut late_rx) = mpsc::unbounded_channel();
    let early_backpressure = ActorInboxBackpressure::default();
    let late_backpressure = ActorInboxBackpressure::default();
    let early = load_actor_worker(&make_early, early_live, &mut early_rx, &early_backpressure);
    let late = load_actor_worker(&make_late, late_live, &mut late_rx, &late_backpressure);
    tokio::pin!(early, late);
    let started = Instant::now();
    // Poll both real loads to their first failed persistence read and arm
    // both backoff sleeps at the SAME paused instant before advancing time.
    assert!(poll!(early.as_mut()).is_pending());
    assert!(poll!(late.as_mut()).is_pending());
    assert!(
        early_store.rooms_errors.lock().unwrap().is_empty(),
        "early first load did not reach persistence"
    );
    assert!(
        late_store.rooms_errors.lock().unwrap().is_empty(),
        "late first load did not reach persistence"
    );
    assert!(early_reads.lock().unwrap().is_empty());
    assert!(late_reads.lock().unwrap().is_empty());
    assert_eq!(started.elapsed(), Duration::ZERO);

    advance(early_wait - Duration::from_millis(1)).await;
    assert!(
        poll!(early.as_mut()).is_pending(),
        "early guild retried before its jittered offset"
    );
    assert!(poll!(late.as_mut()).is_pending());
    advance(Duration::from_millis(3)).await;
    assert!(
        matches!(poll!(early.as_mut()), std::task::Poll::Ready(Some(_))),
        "early guild must retry its actual persistence load at {early_wait:?}, not the deterministic 1 s base"
    );
    assert!(
        poll!(late.as_mut()).is_pending(),
        "guild actors must not retry in lockstep"
    );
    assert!(late_reads.lock().unwrap().is_empty());

    let late_target = started + late_wait + Duration::from_millis(2);
    advance(late_target.saturating_duration_since(Instant::now())).await;
    assert!(
        matches!(poll!(late.as_mut()), std::task::Poll::Ready(Some(_))),
        "late guild must retry its actual persistence load at {late_wait:?}"
    );
    for (reads, wait) in [(&early_reads, early_wait), (&late_reads, late_wait)] {
        let reads = reads.lock().unwrap();
        assert_eq!(
            reads.len(),
            1,
            "exactly one successful second load, after the scripted first failure"
        );
        let elapsed = reads[0].duration_since(started);
        assert!(
            elapsed >= wait && elapsed <= wait + Duration::from_millis(2),
            "second persistence read at {elapsed:?}, expected {wait:?} plus timer rounding"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "deterministic first-failure backoff was restored"
        );
    }
    assert_ne!(
        early_reads.lock().unwrap()[0],
        late_reads.lock().unwrap()[0]
    );
    assert!(
        early_trace.lock().unwrap().is_empty(),
        "initialization must not write to Discord"
    );
    assert!(
        late_trace.lock().unwrap().is_empty(),
        "initialization must not write to Discord"
    );
}
