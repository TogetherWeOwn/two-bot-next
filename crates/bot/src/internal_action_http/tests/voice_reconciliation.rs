#![cfg(test)]
//! Real `VoiceMoveExecutor::execute_move` against the real gateway pipeline.
//! A test-only barrier publishes the member's source -> target change after the
//! executor's final occupancy read and before it reserves the seat, with the
//! observer callback paused. The PATCH then answers uncertainly (502), so only
//! a generation-fenced observation may ever release the hold.
use super::*;
use crate::voice_move::occupancy;
use two_bot_discord::{VoiceTransitionObservation, VoiceTransitionObserver};

/// Observer that queues callbacks so the test controls their delivery, while
/// still taking the real generation fence when the pipeline publishes.
struct PausedObserver {
    clock: Arc<VoiceOccupancyClock>,
    pending: Mutex<Vec<VoiceTransitionObservation>>,
}

impl VoiceTransitionObserver for PausedObserver {
    fn begin_voice_transition(&self) -> u64 {
        self.clock.begin_voice_transition()
    }

    fn observe_voice_transition(&self, transition: VoiceTransitionObservation) {
        self.pending.lock().unwrap().push(transition);
    }

    fn reset_voice_occupancy(&self, guild_id: Option<u64>, channel_id: Option<u64>) {
        self.clock.reset_voice_occupancy(guild_id, channel_id);
    }
}

impl PausedObserver {
    fn deliver(&self) {
        let queued: Vec<_> = self.pending.lock().unwrap().drain(..).collect();
        for transition in queued {
            self.clock.observe_voice_transition(transition);
        }
    }

    fn queued(&self) -> usize {
        self.pending.lock().unwrap().len()
    }
}

const ROOM_C: &str = "100000000000000033";

fn publish(pipeline: &two_bot_discord::MemPipeline, user: &str, channel: &str, at: &str) {
    pipeline.handle_at(
        &Event::VoiceStateUpdate(Box::new(voice_state(user, Some(channel)))),
        at,
    );
}

#[tokio::test]
async fn real_executor_publication_before_reserve_keeps_hold_until_fenced_departure() {
    crate::gateway::ensure_crypto_provider();
    let mock = MockVoiceApi::start(MockVoiceApiConfig {
        patch_status: 502,
        ..MockVoiceApiConfig::default()
    })
    .await;
    let cache = seed_voice_move_cache(voice_snapshot::new_shared_cache(), VOICE_MOVE_MEMBER_ID);
    let clock = VoiceOccupancyClock::new();
    let observer = Arc::new(PausedObserver {
        clock: clock.clone(),
        pending: Mutex::new(Vec::new()),
    });
    let pipeline = Arc::new(two_bot_discord::MemPipeline::with_shared_cache(
        two_bot_core::MemStore::new(),
        Some(two_bot_core::NoopLeveling),
        Some(two_bot_core::NoopFacts),
        two_bot_discord::ScriptedInvites::new(),
        two_bot_discord::NoClassification,
        two_bot_discord::PipelineSnapshots::new(),
        Arc::clone(&cache),
    ));
    pipeline.set_voice_transition_observer(observer.clone());
    let discord =
        ActionExecutor::with_proxy("not-a-credential".to_owned(), Some(mock.origin.clone()))
            .unwrap();
    let executor = VoiceMoveExecutor::new(
        discord,
        Arc::clone(&cache),
        connected_gateway(),
        clock.clone(),
    );
    // Barrier: after the final read and decision, before the reserve, the
    // gateway publishes M source -> target; its observer callback stays paused.
    let barrier_pipeline = Arc::clone(&pipeline);
    executor.set_before_reserve(move || {
        publish(
            &barrier_pipeline,
            VOICE_MOVE_MEMBER_ID,
            VOICE_MOVE_TARGET_ID,
            "2026-11-03T10:00:00Z",
        );
    });

    let guild: Snowflake = staging_guild().parse().unwrap();
    let member: Snowflake = VOICE_MOVE_MEMBER_ID.parse().unwrap();
    let second: Snowflake = VOICE_MOVE_SECOND_ID.parse().unwrap();
    let target: Snowflake = VOICE_MOVE_TARGET_ID.parse().unwrap();
    let read = |user: Snowflake| {
        let mut admissions = clock.move_admissions.lock().unwrap();
        occupancy(&cache, guild, user, target, &mut admissions)
    };
    let held = || {
        clock
            .move_admissions
            .lock()
            .unwrap()
            .reserve(member, target)
    };

    let body = voice_move_body(VOICE_MOVE_MEMBER_ID, VOICE_MOVE_TARGET_ID);
    let request = VoiceMoveRequest::validate(&body).expect("shaped move validates");
    let error = executor
        .execute_move(&request)
        .await
        .expect_err("a 502 PATCH is uncertain, not a receipt");
    assert_eq!(error.code, ErrorCode::DiscordUnavailable);
    assert_eq!(error.log_reason, "voice_move_discord_uncertain");
    assert_eq!(mock.patch_calls(), 1);
    assert_eq!(observer.queued(), 1, "the barrier publication is paused");

    // The cache already shows M in the target, yet equality must not settle
    // the uncertain admission: two cached occupants plus the held seat.
    assert_eq!(read(member), Err(()));
    assert_eq!(read(second), Ok((Some(target), 3)));

    // The delayed callback predates the reservation, so it cannot clear it.
    observer.deliver();
    assert_eq!(read(member), Err(()));
    assert_eq!(read(second), Ok((Some(target), 3)));
    assert!(held().is_err());

    // A real second request for M to a different target is fenced too, not
    // just a repeat of the uncertain move into B.
    let other_body = voice_move_body(VOICE_MOVE_MEMBER_ID, VOICE_MOVE_SOURCE_ID);
    let other_request = VoiceMoveRequest::validate(&other_body).unwrap();
    let retry = executor
        .execute_move(&other_request)
        .await
        .expect_err("held member fails closed");
    assert_eq!(retry.log_reason, "voice_move_cache_race");
    assert_eq!(mock.patch_calls(), 1);

    // A newer global event for another member does not clear M's hold.
    publish(
        &pipeline,
        VOICE_MOVE_SECOND_ID,
        ROOM_C,
        "2026-11-03T10:00:01Z",
    );
    observer.deliver();
    assert_eq!(read(member), Err(()));
    assert_eq!(read(second), Ok((Some(ROOM_C.parse().unwrap()), 2)));
    assert!(held().is_err());

    // M -> C is published but its callback has not run: the target seat is
    // still held although the cache no longer shows anyone in the target.
    publish(
        &pipeline,
        VOICE_MOVE_MEMBER_ID,
        ROOM_C,
        "2026-11-03T10:00:02Z",
    );
    assert_eq!(read(second), Ok((Some(ROOM_C.parse().unwrap()), 1)));
    assert!(held().is_err());

    // The proper later departure from the target settles it exactly once.
    observer.deliver();
    assert_eq!(read(second), Ok((Some(ROOM_C.parse().unwrap()), 0)));
    assert_eq!(read(member), Ok((Some(ROOM_C.parse().unwrap()), 0)));
    assert!(held().is_ok());
}
