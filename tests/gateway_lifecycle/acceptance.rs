// Protected offline acceptance template. prepare.py inserts current-source
// scheduling, cache consumers and recovery items at the markers below.
// Only persistence, wall clock and the durable-queue row are test doubles.

mod flush_adapter {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::{oneshot, watch};
    use two_bot::voice_rooms::game_activity::{
        flush_due, GameActivityCollector, GAME_ACTIVITY_GAP_TIMEOUT_MS,
    };
    use two_bot_core::game_activity::{
        ActivitySnapshot, ActivityType, GuildGameAggregate, PresenceDelta,
    };
    use two_bot_core::game_activity_store::GameActivityStoreError;
    use two_bot_core::Snowflake;

    const T0: i64 = 1_750_000_000_000;
    thread_local! {
        static ORIGIN: Cell<Option<tokio::time::Instant>> = const { Cell::new(None) };
    }

    fn now_ms() -> i64 {
        ORIGIN.with(|origin| {
            T0 + i64::try_from(
                tokio::time::Instant::now()
                    .duration_since(origin.get().expect("fixture clock initialized"))
                    .as_millis(),
            )
            .expect("fixture elapsed time fits")
        })
    }

    #[derive(Clone, Default)]
    pub struct Sink {
        rows: Arc<Mutex<Vec<GuildGameAggregate>>>,
        pause: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    }

    mod sqlx {
        pub(super) type PgPool = super::Sink;
    }

    // Real collector drain/sessionization; only the database write is replaced.
    async fn persist_guild(
        pool: &sqlx::PgPool,
        collector: &GameActivityCollector,
        guild: Snowflake,
        gap_timeout_ms: i64,
        now_ms: i64,
    ) -> Result<(), GameActivityStoreError> {
        let pause = pool.pause.lock().expect("sink lock").take();
        if let Some(pause) = pause {
            pause.await.expect("release persistence");
        }
        pool.rows
            .lock()
            .expect("sink lock")
            .extend(collector.flush(guild, gap_timeout_ms, now_ms));
        Ok(())
    }

    // SCHEDULING_SOURCE

    fn fixture() -> (
        Sink,
        Arc<GameActivityCollector>,
        watch::Sender<bool>,
        watch::Receiver<bool>,
    ) {
        ORIGIN.with(|origin| origin.set(Some(tokio::time::Instant::now())));
        let (tx, rx) = watch::channel(false);
        (
            Sink::default(),
            Arc::new(GameActivityCollector::new()),
            tx,
            rx,
        )
    }

    fn episode(collector: &GameActivityCollector, start: i64) {
        let guild: Snowflake = 111;
        let member: Snowflake = 222;
        collector.observe_join(guild, member, start);
        collector.observe_presence(
            guild,
            PresenceDelta {
                user_key: "222".into(),
                observed_at_ms: start,
                activities: vec![ActivitySnapshot {
                    activity_type: ActivityType::Playing,
                    name: "Fixture Game".into(),
                    application_id: None,
                    details: None,
                    state: None,
                    party_size: None,
                    assets: false,
                    button_labels: vec![],
                    button_urls: vec![],
                    stream_url: None,
                }],
            },
        );
        collector.observe_presence(
            guild,
            PresenceDelta {
                user_key: "222".into(),
                observed_at_ms: start + 120_000,
                activities: vec![],
            },
        );
        collector.observe_leave(guild, member, start + 120_000);
    }

    async fn settle() {
        for _ in 0..12 {
            tokio::task::yield_now().await;
        }
    }

    async fn finish(task: tokio::task::JoinHandle<()>) -> bool {
        // Paused-time join: a wall-clock timeout never fires without an
        // explicit advance, so yield the loop onto the latch, advance past
        // the join budget, then read is_finished. The 1s advance stays
        // inside the 300s poll interval, so it cannot trigger a periodic
        // flush and skew the totals assertions.
        settle().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        if task.is_finished() {
            task.await.expect("flush task did not panic");
            true
        } else {
            task.abort();
            assert!(task
                .await
                .expect_err("aborted baseline loop")
                .is_cancelled());
            false
        }
    }

    fn totals(sink: &Sink) -> (i64, i64, u64) {
        sink.rows
            .lock()
            .expect("sink lock")
            .iter()
            .fold((0, 0, 0), |sum, row| {
                (
                    sum.0 + row.total_play_ms,
                    sum.1 + row.in_voice_ms,
                    sum.2 + row.session_count,
                )
            })
    }

    // Catches dropping a partial hourly interval on the first shutdown latch.
    #[tokio::test(start_paused = true)]
    async fn shutdown_persists_one_partial_interval_and_exits() {
        let (sink, collector, shutdown, receiver) = fixture();
        episode(&collector, T0);
        let task = tokio::spawn(launch(sink.clone(), collector, receiver));
        settle().await;
        tokio::time::advance(Duration::from_secs(180)).await;
        shutdown.send(true).expect("loop owns receiver");
        let exited = finish(task).await;
        assert_eq!(
            totals(&sink),
            (120_000, 120_000, 1),
            "shutdown dropped the buffered partial interval"
        );
        assert!(exited, "shutdown did not join the flush loop");
    }

    // Catches waiting for changed() without checking an already-latched value.
    #[tokio::test(start_paused = true)]
    async fn prelatched_shutdown_flushes_without_waiting_for_another_signal() {
        let (sink, collector, shutdown, receiver) = fixture();
        episode(&collector, T0);
        tokio::time::advance(Duration::from_secs(180)).await;
        shutdown.send(true).expect("receiver exists");
        let exited = finish(tokio::spawn(launch(sink.clone(), collector, receiver))).await;
        assert_eq!(
            totals(&sink),
            (120_000, 120_000, 1),
            "prelatched shutdown lost buffered activity"
        );
        assert!(exited, "prelatched shutdown was ignored");
    }

    // Catches both suppressing the tail after a periodic write and recounting
    // the already-drained first interval when the final write is forced.
    #[tokio::test(start_paused = true)]
    async fn periodic_then_shutdown_counts_each_interval_once() {
        let (sink, collector, shutdown, receiver) = fixture();
        episode(&collector, T0);
        let task = tokio::spawn(launch(sink.clone(), Arc::clone(&collector), receiver));
        settle().await;
        tokio::time::advance(Duration::from_secs(3600)).await;
        settle().await;
        assert_eq!(
            totals(&sink),
            (120_000, 120_000, 1),
            "ordinary hourly flush changed"
        );
        episode(&collector, T0 + 3_600_000);
        tokio::time::advance(Duration::from_secs(180)).await;
        shutdown.send(true).expect("loop owns receiver");
        let exited = finish(task).await;
        assert_eq!(
            totals(&sink),
            (240_000, 240_000, 2),
            "shutdown must drain only the remaining interval"
        );
        assert!(exited, "shutdown did not join the flush loop");
    }

    // Catches cancelling an admitted persistence future on shutdown, or
    // flushing its already-consumed buffers a second time after it completes.
    #[tokio::test(start_paused = true)]
    async fn shutdown_during_persist_waits_for_it_without_double_counting() {
        let (sink, collector, shutdown, receiver) = fixture();
        episode(&collector, T0);
        let task = tokio::spawn(launch(sink.clone(), collector, receiver));
        settle().await;
        let (release, held) = oneshot::channel();
        *sink.pause.lock().expect("sink lock") = Some(held);
        tokio::time::advance(Duration::from_secs(3600)).await;
        settle().await;
        assert!(
            sink.pause.lock().expect("sink lock").is_none(),
            "periodic persist was not entered"
        );
        shutdown.send(true).expect("loop owns receiver");
        release
            .send(())
            .expect("in-flight persist must not be cancelled");
        let exited = finish(task).await;
        assert_eq!(totals(&sink), (120_000, 120_000, 1));
        assert!(
            exited,
            "shutdown ignored after the in-flight write completed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn control_hourly_due_check_preserves_cadence_and_drains_once() {
        let (sink, collector, _, _) = fixture();
        episode(&collector, T0);
        let mut last = HashMap::new();
        flush_due_guilds(
            &sink,
            &collector,
            &mut last,
            T0,
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
        )
        .await;
        flush_due_guilds(
            &sink,
            &collector,
            &mut last,
            T0 + 3_599_999,
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
        )
        .await;
        assert_eq!(totals(&sink), (0, 0, 0));
        flush_due_guilds(
            &sink,
            &collector,
            &mut last,
            T0 + 3_600_000,
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
        )
        .await;
        assert_eq!(totals(&sink), (120_000, 120_000, 1));
        flush_due_guilds(
            &sink,
            &collector,
            &mut last,
            T0 + 3_600_000,
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
        )
        .await;
        flush_due_guilds(
            &sink,
            &collector,
            &mut last,
            T0 + 7_200_000,
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
        )
        .await;
        assert_eq!(totals(&sink), (120_000, 120_000, 1));
    }
}

mod cache_adapter {
    use std::collections::*;
    use std::sync::*;
    use twilight_model::application::interaction::Interaction;
    use two_bot_core::onboarding::MembershipTrigger;

    // RECOVERY_SOURCE
    // CACHE_SOURCE

    fn commit(
        writer_live: &LiveInteractions,
        id: i64,
        interaction: Box<Interaction>,
        ticket: tokio::sync::oneshot::Receiver<bool>,
        observed_generation: u64,
    ) {
        // INSERT_SOURCE
    }

    struct Saved {
        id: i64,
        payload: String,
    }

    fn claim(
        live_interactions: &LiveInteractions,
        saved: &Saved,
    ) -> Result<(Option<LiveInteraction>, Option<OnboardingJob>), sqlx::Error> {
        // CLAIM_SOURCE
        Ok((live, job))
    }

    fn interaction(id: u64) -> Box<Interaction> {
        // Same proven shape as the gateway slash() packet: only the token
        // differs, so the durable-marker assertion can detect leakage.
        Box::new(
            serde_json::from_value(serde_json::json!({
                "id": id.to_string(), "application_id": "1111", "type": 2,
                "token": "synthetic-interaction-token", "version": 1,
                "guild_id": "22",
                "channel": {"id": "4444", "type": 0, "name": "commands"},
                "authorizing_integration_owners": {"0": "22"}, "entitlements": [],
                "member": {
                    "permissions": "0", "roles": [], "joined_at": null,
                    "deaf": false, "mute": false, "flags": 0,
                    "user": {"id": "3333", "username": "tester",
                             "discriminator": "0000", "avatar": null}
                },
                "data": {"id": "5555", "name": "fixture", "type": 1, "options": []}
            }))
            .expect("synthetic interaction"),
        )
    }

    // Explicit test budget: production may choose a smaller cap, not a larger
    // unbounded working set. Descending IDs distinguish arrival order from key
    // order. Keeping all senders lets eviction prove the live ticket was freed.
    const BUDGET: usize = 1024;
    const BURST: usize = 1536;

    fn burst() -> (LiveInteractions, Vec<tokio::sync::oneshot::Sender<bool>>) {
        let live = LiveInteractions::default();
        let mut tickets = Vec::new();
        for index in 0..BURST {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            commit(
                &live,
                10_000 - index as i64,
                interaction(1_000_000 + index as u64),
                receiver,
                7,
            );
            tickets.push(sender);
        }
        (live, tickets)
    }

    #[test]
    fn burst_is_bounded_and_keeps_exactly_the_newest_suffix() {
        let (live, _tickets) = burst();
        let cache = live.lock().expect("cache lock");
        let retained = cache.len();
        assert!(retained > 0 && retained <= BUDGET, "live cache retained {retained} entries after a {BURST}-entry burst; budget is {BUDGET}");
        for index in 0..BURST {
            assert_eq!(
                cache.contains_key(&(10_000 - index as i64)),
                index >= BURST - retained,
                "eviction must follow arrival order, not numeric job ID"
            );
        }
    }

    #[test]
    fn oldest_eviction_releases_the_original_ack_ticket() {
        let (_live, mut tickets) = burst();
        assert!(
            tickets.remove(0).send(true).is_err(),
            "oldest interaction and its ACK ticket were not evicted"
        );
        assert!(
            tickets.pop().expect("newest ticket").send(true).is_ok(),
            "newest live interaction was discarded"
        );
    }

    #[test]
    fn evicted_claim_uses_durable_recovery_without_replaying_credentials() {
        let (live, _tickets) = burst();
        let payload = OnboardingJob::Interaction(interaction(1_000_000))
            .durable_payload()
            .expect("durable marker");
        assert!(!payload.contains("synthetic-interaction-token"));
        let (cached, recovered) = claim(
            &live,
            &Saved {
                id: 10_000,
                payload,
            },
        )
        .expect("recover oldest claim");
        assert!(
            cached.is_none(),
            "oldest claim incorrectly retained the live credential-bearing interaction"
        );
        assert!(
            recovered.is_none(),
            "interrupted interaction marker must not become replayable work"
        );
    }

    #[tokio::test]
    async fn control_retained_claim_preserves_interaction_ticket_and_generation() {
        let live = LiveInteractions::default();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        commit(&live, 50, interaction(999), receiver, 23);
        let (cached, recovered) = claim(
            &live,
            &Saved {
                id: 50,
                payload: "not JSON; live path must win".into(),
            },
        )
        .expect("live claim");
        let cached = cached.expect("retained live interaction");
        assert_eq!(cached.interaction.id.get(), 999);
        assert_eq!(cached.generation, 23);
        assert!(
            matches!(recovered, Some(OnboardingJob::Interaction(value)) if value.id.get() == 999)
        );
        sender.send(true).expect("original receiver retained");
        assert!(cached.ticket.await.expect("original ticket delivered"));
        assert!(live.lock().expect("cache lock").is_empty());
    }

    #[test]
    fn control_missing_claim_recovers_durable_member_work() {
        let live = LiveInteractions::default();
        // Durable member-work payload as persisted JSON (not a struct literal):
        // the spliced enum gains optional fields over time, and recovery
        // accepts payloads captured before they existed.
        let payload =
            r#"{"Goodbye":{"guild_id":111,"username":"fixture","bot":false,"joined_at_ms":123}}"#
                .to_string();
        let (cached, recovered) =
            claim(&live, &Saved { id: 51, payload }).expect("recover member work");
        assert!(cached.is_none());
        assert!(matches!(
            recovered,
            Some(OnboardingJob::Goodbye {
                guild_id: 111,
                bot: false,
                joined_at_ms: Some(123),
                ..
            })
        ));
        assert!(claim(
            &live,
            &Saved {
                id: 52,
                payload: "not JSON".into()
            }
        )
        .is_err());
    }
}
