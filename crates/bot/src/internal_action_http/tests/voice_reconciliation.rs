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

/// Pause exactly the PATCH's real durable admission, after reservation and
/// before wire construction. All prerequisite reads use real granted permits.
#[derive(Debug)]
struct PausedPatchAdmission {
    key: two_bot_core::send_admission::TokenKey,
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    proceed: tokio::sync::Semaphore,
    error: Option<two_bot_core::send_admission::AdmissionError>,
}

impl two_bot_core::send_admission::SendAdmission for PausedPatchAdmission {
    fn token_key(&self) -> &two_bot_core::send_admission::TokenKey {
        &self.key
    }

    fn admit(
        &self,
    ) -> two_bot_core::send_admission::AdmissionFuture<
        '_,
        Result<
            two_bot_core::send_admission::AdmissionPermit,
            two_bot_core::send_admission::AdmissionError,
        >,
    > {
        let pause = self.armed.swap(false, Ordering::SeqCst);
        Box::pin(async move {
            if pause {
                self.entered.notify_one();
                self.proceed.acquire().await.unwrap().forget();
                if let Some(error) = self.error {
                    return Err(error);
                }
            }
            Ok(two_bot_core::send_admission::AdmissionPermit::new(
                Box::new(GrantedCompletion(Arc::new(SettlementEvidence::default()))),
            ))
        })
    }
}

struct PendingFixture {
    mock: MockVoiceApi,
    cache: Arc<DefaultInMemoryCache>,
    clock: Arc<VoiceOccupancyClock>,
    observer: Arc<PausedObserver>,
    pipeline: two_bot_discord::MemPipeline,
    admission: Arc<PausedPatchAdmission>,
    executor: VoiceMoveExecutor,
}

impl PendingFixture {
    async fn new(
        config: MockVoiceApiConfig,
        error: Option<two_bot_core::send_admission::AdmissionError>,
    ) -> Self {
        crate::gateway::ensure_crypto_provider();
        let mock = MockVoiceApi::start(config).await;
        let cache = seeded_voice_move_cache();
        let clock = VoiceOccupancyClock::new();
        let observer = Arc::new(PausedObserver {
            clock: clock.clone(),
            pending: Mutex::new(Vec::new()),
        });
        let pipeline = two_bot_discord::MemPipeline::with_shared_cache(
            two_bot_core::MemStore::new(),
            Some(two_bot_core::NoopLeveling),
            Some(two_bot_core::NoopFacts),
            two_bot_discord::ScriptedInvites::new(),
            two_bot_discord::NoClassification,
            two_bot_discord::PipelineSnapshots::new(),
            cache.clone(),
        );
        pipeline.set_voice_transition_observer(observer.clone());
        publish(
            &pipeline,
            VOICE_MOVE_SECOND_ID,
            VOICE_MOVE_SOURCE_ID,
            "2026-11-03T09:00:00Z",
        );
        observer.deliver();
        const TOKEN: &str = "not-a-credential";
        let admission = Arc::new(PausedPatchAdmission {
            key: two_bot_core::send_admission::TokenKey::for_bot_token(TOKEN).unwrap(),
            armed: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            proceed: tokio::sync::Semaphore::new(0),
            error,
        });
        let discord = ActionExecutor::with_admission(
            TOKEN.to_owned(),
            Some(mock.origin.clone()),
            admission.clone(),
        )
        .unwrap();
        let executor =
            VoiceMoveExecutor::new(discord, cache.clone(), connected_gateway(), clock.clone());
        Self {
            mock,
            cache,
            clock,
            observer,
            pipeline,
            admission,
            executor,
        }
    }

    fn read(&self, member: &str) -> Result<(Option<u64>, u32), ()> {
        let mut admissions = self.clock.move_admissions.lock().unwrap();
        occupancy(
            &self.cache,
            staging_guild().parse().unwrap(),
            member.parse().unwrap(),
            VOICE_MOVE_TARGET_ID.parse().unwrap(),
            &mut admissions,
        )
    }

    fn arm(&self) {
        let admission = self.admission.clone();
        self.executor.set_before_reserve(move || {
            admission.armed.store(true, Ordering::SeqCst);
        });
    }

    async fn run(
        &self,
        request: &VoiceMoveRequest<'_>,
        while_pending: impl FnOnce(),
    ) -> Result<VoiceMoveReceipt, ActionError> {
        self.arm();
        let mut operation = self.executor.execute_move(request);
        tokio::time::timeout(BROKEN_PATCH_BOUND, async {
            tokio::select! {
                _ = self.admission.entered.notified() => {},
                result = &mut operation => panic!("PATCH did not reach paused admission: {result:?}"),
            }
        }).await.expect("real PATCH reaches admission within the test bound");
        assert_eq!(self.mock.patch_calls(), 0, "reservation precedes the wire");
        while_pending();
        self.admission.proceed.add_permits(1);
        tokio::time::timeout(BROKEN_PATCH_BOUND, operation)
            .await
            .unwrap()
    }
}

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

#[tokio::test]
async fn real_executor_pending_patch_and_delayed_callbacks_keep_the_last_seat() {
    for status in [200, 502] {
        for delayed in [false, true] {
            let effect = Arc::new(Mutex::new(None));
            let fixture = PendingFixture::new(
                MockVoiceApiConfig {
                    patch_status: status,
                    target_limit: 1,
                    patch_effect: Some(effect.clone()),
                    ..MockVoiceApiConfig::default()
                },
                None,
            )
            .await;
            let body = voice_move_body(VOICE_MOVE_MEMBER_ID, VOICE_MOVE_TARGET_ID);
            let request = VoiceMoveRequest::validate(&body).unwrap();
            let result = fixture
                .run(&request, || {
                    // Independent A -> B -> C while the real PATCH still waits
                    // before wire construction must not retire Pending.
                    publish(
                        &fixture.pipeline,
                        VOICE_MOVE_MEMBER_ID,
                        VOICE_MOVE_TARGET_ID,
                        "2026-11-03T10:00:00Z",
                    );
                    publish(
                        &fixture.pipeline,
                        VOICE_MOVE_MEMBER_ID,
                        ROOM_C,
                        "2026-11-03T10:00:01Z",
                    );
                    assert_eq!(fixture.observer.queued(), 2);
                    if !delayed {
                        fixture.observer.deliver();
                    }
                    assert_eq!(fixture.read(VOICE_MOVE_MEMBER_ID), Err(()));
                    assert_eq!(
                        fixture.read(VOICE_MOVE_SECOND_ID),
                        Ok((Some(VOICE_MOVE_SOURCE_ID.parse().unwrap()), 1))
                    );
                })
                .await;
            if status == 200 {
                assert_eq!(result.unwrap().outcome, VoiceMoveOutcome::Moved);
            } else {
                assert_eq!(
                    result.unwrap_err().log_reason,
                    "voice_move_discord_uncertain"
                );
            }
            assert_eq!(fixture.mock.patch_calls(), 1);
            assert_eq!(
                effect.lock().unwrap().as_deref(),
                Some(VOICE_MOVE_TARGET_ID)
            );
            // Synthetic Discord effect is B, but its gateway publication is
            // delayed: the cache still shows C. Pre-completion callbacks must
            // remain ineligible when delivered after the completion fence.
            fixture.observer.deliver();
            assert_eq!(fixture.read(VOICE_MOVE_MEMBER_ID), Err(()));
            assert_eq!(
                fixture.read(VOICE_MOVE_SECOND_ID),
                Ok((Some(VOICE_MOVE_SOURCE_ID.parse().unwrap()), 1))
            );
            let other_body = voice_move_body(VOICE_MOVE_SECOND_ID, VOICE_MOVE_TARGET_ID);
            let other = VoiceMoveRequest::validate(&other_body).unwrap();
            assert_eq!(
                move_within_bound(&fixture.executor, &other)
                    .await
                    .unwrap()
                    .outcome,
                VoiceMoveOutcome::RoomFull
            );
            let retry_body = voice_move_body(VOICE_MOVE_MEMBER_ID, VOICE_MOVE_SOURCE_ID);
            let retry = VoiceMoveRequest::validate(&retry_body).unwrap();
            assert_eq!(
                move_within_bound(&fixture.executor, &retry)
                    .await
                    .unwrap_err()
                    .log_reason,
                "voice_move_cache_race"
            );
            assert_eq!(
                fixture.mock.patch_calls(),
                1,
                "neither refused receiver request sent a second PATCH"
            );

            // Begin-after-completion is the other mutex ordering: genuine M
            // arrival clears only its hold, leaving exactly one cached seat.
            publish(
                &fixture.pipeline,
                VOICE_MOVE_MEMBER_ID,
                VOICE_MOVE_TARGET_ID,
                "2026-11-03T10:00:02Z",
            );
            fixture.observer.deliver();
            assert_eq!(
                fixture.read(VOICE_MOVE_MEMBER_ID),
                Ok((Some(VOICE_MOVE_TARGET_ID.parse().unwrap()), 1))
            );
            assert_eq!(
                fixture.read(VOICE_MOVE_SECOND_ID),
                Ok((Some(VOICE_MOVE_SOURCE_ID.parse().unwrap()), 1))
            );
        }
    }
}

#[tokio::test]
async fn real_executor_early_patch_arrival_is_not_replayed_at_completion() {
    for status in [200, 502] {
        let barrier = Arc::new(PatchResponseBarrier {
            entered: tokio::sync::Notify::new(),
            proceed: tokio::sync::Semaphore::new(0),
        });
        let effect = Arc::new(Mutex::new(None));
        let fixture = PendingFixture::new(
            MockVoiceApiConfig {
                patch_status: status,
                patch_effect: Some(effect.clone()),
                patch_response_barrier: Some(barrier.clone()),
                ..MockVoiceApiConfig::default()
            },
            None,
        )
        .await;
        let body = voice_move_body(VOICE_MOVE_MEMBER_ID, VOICE_MOVE_TARGET_ID);
        let request = VoiceMoveRequest::validate(&body).unwrap();
        // Do not arm the pre-wire barrier: the synthetic server must process
        // the real PATCH before we publish its arrival, holding only its reply.
        let mut operation = fixture.executor.execute_move(&request);
        tokio::time::timeout(BROKEN_PATCH_BOUND, async {
            tokio::select! {
                _ = barrier.entered.notified() => {},
                result = &mut operation => panic!("PATCH response did not pause: {result:?}"),
            }
        })
        .await
        .unwrap();
        assert_eq!(fixture.mock.patch_calls(), 1);
        assert_eq!(
            effect.lock().unwrap().as_deref(),
            Some(VOICE_MOVE_TARGET_ID)
        );
        publish(
            &fixture.pipeline,
            VOICE_MOVE_MEMBER_ID,
            VOICE_MOVE_TARGET_ID,
            "2026-11-03T10:00:00Z",
        );
        fixture.observer.deliver();
        assert_eq!(fixture.read(VOICE_MOVE_MEMBER_ID), Err(()));
        assert_eq!(
            fixture.read(VOICE_MOVE_SECOND_ID),
            Ok((Some(VOICE_MOVE_SOURCE_ID.parse().unwrap()), 2))
        );
        barrier.proceed.add_permits(1);
        let result = tokio::time::timeout(BROKEN_PATCH_BOUND, operation)
            .await
            .unwrap();
        if status == 200 {
            assert_eq!(result.unwrap().outcome, VoiceMoveOutcome::Moved);
        } else {
            assert_eq!(
                result.unwrap_err().log_reason,
                "voice_move_discord_uncertain"
            );
        }
        assert_eq!(fixture.read(VOICE_MOVE_MEMBER_ID), Err(()));
        assert_eq!(
            fixture.read(VOICE_MOVE_SECOND_ID),
            Ok((Some(VOICE_MOVE_SOURCE_ID.parse().unwrap()), 2))
        );
        fixture.pipeline.handle_at(
            &Event::VoiceStateUpdate(Box::new(voice_state(VOICE_MOVE_MEMBER_ID, None))),
            "2026-11-03T10:00:01Z",
        );
        fixture.observer.deliver();
        assert_eq!(fixture.read(VOICE_MOVE_MEMBER_ID), Ok((None, 0)));
        assert_eq!(
            fixture.read(VOICE_MOVE_SECOND_ID),
            Ok((Some(VOICE_MOVE_SOURCE_ID.parse().unwrap()), 0))
        );
        assert_eq!(fixture.mock.patch_calls(), 1);
    }
}

#[tokio::test]
async fn real_executor_generic_error_arms_an_observational_hold() {
    let fixture = PendingFixture::new(
        MockVoiceApiConfig::default(),
        Some(two_bot_core::send_admission::AdmissionError::Configuration),
    )
    .await;
    let body = voice_move_body(VOICE_MOVE_MEMBER_ID, VOICE_MOVE_TARGET_ID);
    let request = VoiceMoveRequest::validate(&body).unwrap();
    let result = fixture
        .run(&request, || {
            publish(
                &fixture.pipeline,
                VOICE_MOVE_MEMBER_ID,
                VOICE_MOVE_TARGET_ID,
                "2026-11-03T10:00:00Z",
            );
            publish(
                &fixture.pipeline,
                VOICE_MOVE_MEMBER_ID,
                VOICE_MOVE_SOURCE_ID,
                "2026-11-03T10:00:01Z",
            );
        })
        .await;
    assert_eq!(result.unwrap_err().code, ErrorCode::DiscordUnavailable);
    fixture.observer.deliver();
    assert_eq!(fixture.mock.patch_calls(), 0);
    assert_eq!(fixture.read(VOICE_MOVE_MEMBER_ID), Err(()));
    assert_eq!(
        fixture.read(VOICE_MOVE_SECOND_ID),
        Ok((Some(VOICE_MOVE_SOURCE_ID.parse().unwrap()), 1))
    );
    // A generic error retained and armed the hold rather than releasing it or
    // leaving it Pending forever. Only this later publication may settle it.
    publish(
        &fixture.pipeline,
        VOICE_MOVE_MEMBER_ID,
        VOICE_MOVE_TARGET_ID,
        "2026-11-03T10:00:02Z",
    );
    fixture.observer.deliver();
    assert_eq!(
        fixture.read(VOICE_MOVE_MEMBER_ID),
        Ok((Some(VOICE_MOVE_TARGET_ID.parse().unwrap()), 1))
    );
}

#[tokio::test]
async fn real_executor_cancelled_attempt_stays_pending() {
    let fixture = PendingFixture::new(MockVoiceApiConfig::default(), None).await;
    let body = voice_move_body(VOICE_MOVE_MEMBER_ID, VOICE_MOVE_TARGET_ID);
    let request = VoiceMoveRequest::validate(&body).unwrap();
    fixture.arm();
    let mut operation = fixture.executor.execute_move(&request);
    tokio::time::timeout(BROKEN_PATCH_BOUND, async {
        tokio::select! {
            _ = fixture.admission.entered.notified() => {},
            result = &mut operation => panic!("PATCH did not pause: {result:?}"),
        }
    })
    .await
    .unwrap();
    drop(operation);
    publish(
        &fixture.pipeline,
        VOICE_MOVE_MEMBER_ID,
        VOICE_MOVE_TARGET_ID,
        "2026-11-03T10:00:00Z",
    );
    publish(
        &fixture.pipeline,
        VOICE_MOVE_MEMBER_ID,
        VOICE_MOVE_SOURCE_ID,
        "2026-11-03T10:00:01Z",
    );
    fixture.observer.deliver();
    assert_eq!(fixture.mock.patch_calls(), 0);
    assert_eq!(fixture.read(VOICE_MOVE_MEMBER_ID), Err(()));
    assert_eq!(
        fixture.read(VOICE_MOVE_SECOND_ID),
        Ok((Some(VOICE_MOVE_SOURCE_ID.parse().unwrap()), 1))
    );
}

#[tokio::test]
async fn real_executor_no_effect_completion_releases_despite_pending_observations() {
    use two_bot_core::send_admission::AdmissionError;
    for (status, admission_error) in [
        (200, Some(AdmissionError::Storage)),
        (200, Some(AdmissionError::Blocked)),
        (403, None),
        (400, None),
        (404, None),
        (429, None),
    ] {
        let fixture = PendingFixture::new(
            MockVoiceApiConfig {
                patch_status: status,
                patch_retry_after: (status == 429).then(|| "1".to_owned()),
                ..MockVoiceApiConfig::default()
            },
            admission_error,
        )
        .await;
        let body = voice_move_body(VOICE_MOVE_MEMBER_ID, VOICE_MOVE_TARGET_ID);
        let request = VoiceMoveRequest::validate(&body).unwrap();
        let mut old_callbacks = Vec::new();
        let result = fixture
            .run(&request, || {
                publish(
                    &fixture.pipeline,
                    VOICE_MOVE_MEMBER_ID,
                    VOICE_MOVE_TARGET_ID,
                    "2026-11-03T10:00:00Z",
                );
                publish(
                    &fixture.pipeline,
                    VOICE_MOVE_MEMBER_ID,
                    VOICE_MOVE_SOURCE_ID,
                    "2026-11-03T10:00:01Z",
                );
                // Keep copies to exercise duplicate stale callbacks against the
                // next Pending and next completed hold as well.
                old_callbacks = fixture.observer.pending.lock().unwrap().clone();
                fixture.observer.deliver();
                assert_eq!(fixture.read(VOICE_MOVE_MEMBER_ID), Err(()));
            })
            .await;
        match (status, admission_error) {
            (_, Some(_)) => assert_eq!(result.unwrap_err().log_reason, "voice_move_not_sent"),
            (403, _) => assert_eq!(result.unwrap().outcome, VoiceMoveOutcome::BotNotPermitted),
            (429, _) => assert_eq!(result.unwrap().outcome, VoiceMoveOutcome::RateLimited),
            _ => {
                assert_eq!(result.unwrap_err().code, ErrorCode::DiscordUnavailable);
                assert_eq!(
                    fixture.mock.gather_calls(),
                    2,
                    "one fresh regather, never resend"
                );
            }
        }
        assert_eq!(
            fixture.mock.patch_calls(),
            usize::from(admission_error.is_none())
        );
        assert_eq!(
            fixture.read(VOICE_MOVE_MEMBER_ID),
            Ok((Some(VOICE_MOVE_SOURCE_ID.parse().unwrap()), 0))
        );
        assert_eq!(
            fixture.read(VOICE_MOVE_SECOND_ID),
            Ok((Some(VOICE_MOVE_SOURCE_ID.parse().unwrap()), 0))
        );

        // A subsequent admission owns a fresh Pending phase and fence. Earlier
        // callbacks cannot erase it before or after local terminal completion.
        let member = VOICE_MOVE_MEMBER_ID.parse().unwrap();
        let target = VOICE_MOVE_TARGET_ID.parse().unwrap();
        fixture
            .clock
            .move_admissions
            .lock()
            .unwrap()
            .reserve(member, target)
            .unwrap();
        for transition in &old_callbacks {
            fixture.clock.observe_voice_transition(transition.clone());
        }
        assert_eq!(fixture.read(VOICE_MOVE_MEMBER_ID), Err(()));
        fixture
            .clock
            .move_admissions
            .lock()
            .unwrap()
            .complete(member);
        for transition in old_callbacks {
            fixture.clock.observe_voice_transition(transition);
        }
        assert_eq!(fixture.read(VOICE_MOVE_MEMBER_ID), Err(()));
        assert_eq!(
            fixture.read(VOICE_MOVE_SECOND_ID),
            Ok((Some(VOICE_MOVE_SOURCE_ID.parse().unwrap()), 1))
        );
    }
}
