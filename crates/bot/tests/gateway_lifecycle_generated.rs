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
    ) -> Result<(), ()> {
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

    const FLUSH_POLL_INTERVAL: Duration = Duration::from_secs(300);
    pub async fn flush_due_guilds(
        pool: &sqlx::PgPool,
        collector: &GameActivityCollector,
        last_flush_ms: &mut HashMap<Snowflake, i64>,
        now_ms: i64,
        gap_timeout_ms: i64,
    ) {
        for guild in collector.guilds() {
            let last = last_flush_ms.get(&guild).copied();
            if last.is_none() {
                last_flush_ms.insert(guild, now_ms);
                continue;
            }
            if !flush_due(last, now_ms) {
                continue;
            }
            // The buffers drain whether or not the write lands: a disabled guild
            // drops stale state, and a failed upsert requeues inside the call.
            // Only a gate failure (fail-closed, buffers intact) skips the stamp
            // so the next tick retries instead of dropping an hour.
            if persist_guild(pool, collector, guild, gap_timeout_ms, now_ms)
                .await
                .is_ok()
            {
                last_flush_ms.insert(guild, now_ms);
            }
        }
    }

    /// Persist whatever every guild has buffered, due or not.
    ///
    /// Shutdown-only counterpart of [`flush_due_guilds`]: the final partial
    /// interval is not yet "due", so the hourly gate would skip it. Each guild
    /// goes through the same drain-then-persist path, so a guild the hourly tick
    /// already flushed contributes only what arrived since and is never counted
    /// twice. A gate failure leaves the buffers intact and is not retried: the
    /// process is exiting.
    pub async fn flush_remaining_guilds(
        pool: &sqlx::PgPool,
        collector: &GameActivityCollector,
        now_ms: i64,
        gap_timeout_ms: i64,
    ) {
        for guild in collector.guilds() {
            let _ = persist_guild(pool, collector, guild, gap_timeout_ms, now_ms).await;
        }
    }

    /// Hourly per-guild persist loop for the voice runtime's collector.
    ///
    /// Wakes every `FLUSH_POLL_INTERVAL` and persists each guild due per
    /// [`flush_due`]. When `shutdown` latches (including before the first poll)
    /// the loop runs one trailing [`flush_remaining_guilds`] and returns, so the
    /// caller can join it. A persist already in flight is never cancelled: the
    /// latch is observed only between ticks, and the trailing flush reads the
    /// already-drained buffers, so the interval is counted once.
    pub async fn flush_loop(
        pool: sqlx::PgPool,
        collector: Arc<GameActivityCollector>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut last_flush_ms: HashMap<Snowflake, i64> = HashMap::new();
        let mut tick = tokio::time::interval(FLUSH_POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                // A dropped sender also ends the loop: nothing can latch anymore.
                _ = shutdown.wait_for(|latched| *latched) => break,
                _ = tick.tick() => {}
            }
            flush_due_guilds(
                &pool,
                &collector,
                &mut last_flush_ms,
                now_ms(),
                GAME_ACTIVITY_GAP_TIMEOUT_MS,
            )
            .await;
        }
        flush_remaining_guilds(&pool, &collector, now_ms(), GAME_ACTIVITY_GAP_TIMEOUT_MS).await;
    }

    async fn launch(
        pool: sqlx::PgPool,
        collector: Arc<GameActivityCollector>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let _ = &shutdown;
        flush_loop(pool, collector, shutdown).await;
    }

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

    #[derive(serde::Serialize, serde::Deserialize)]
    pub enum OnboardingJob {
        Welcome {
            guild_id: u64,
            member_id: u64,
            bot: bool,
            pending: bool,
            trigger: MembershipTrigger,
            roles: Vec<String>,
        },
        Goodbye {
            guild_id: u64,
            username: String,
            bot: bool,
            joined_at_ms: Option<i64>,
            /// Who left, so the completed goodbye can release their prompt guard
            /// and a rejoin is welcomed again. `None` for payloads captured
            /// before the clearing path existed (serde leaves a missing `Option`
            /// as `None`): the goodbye still posts, only the clearing is skipped.
            member_id: Option<u64>,
        },
        #[serde(skip)]
        Interaction(Box<Interaction>),
    }
    impl OnboardingJob {
        /// Persist captured member state, but never a component callback credential.
        /// Interrupted components require a fresh member submission after restart;
        /// replaying an uncertain defer or partial role mutation is not safe.
        pub fn durable_payload(&self) -> Result<String, serde_json::Error> {
            match self {
                Self::Interaction(interaction) => serde_json::to_string(&serde_json::json!({
                    "interrupted_interaction": interaction.id.to_string(),
                })),
                _ => serde_json::to_string(self),
            }
        }

        pub fn recover(payload: &str) -> Result<Option<Self>, serde_json::Error> {
            let value: serde_json::Value = serde_json::from_str(payload)?;
            if value["interrupted_interaction"].as_str().is_some() {
                Ok(None)
            } else {
                serde_json::from_value(value).map(Some)
            }
        }
    }

    struct LiveInteraction {
        interaction: Box<twilight_model::application::interaction::Interaction>,
        ticket: tokio::sync::oneshot::Receiver<bool>,
        generation: u64,
    }

    // Upper bound on committed interactions awaiting a worker claim. Claims
    // queue behind `ONBOARDING_WORKER_LIMIT` workers (90 s each), so a sustained
    // burst would otherwise grow the map without limit. An evicted entry is not
    // lost: its claim falls back to the durable `OnboardingJob::recover` path.
    const LIVE_INTERACTION_CAP: usize = 1024;

    /// Arrival-ordered live-interaction cache: at most `LIVE_INTERACTION_CAP`
    /// entries, evicting the oldest arrival on insert. Eviction drops the
    /// interaction and its ACK ticket receiver.
    #[derive(Default)]
    struct LiveCache {
        entries: HashMap<i64, LiveInteraction>,
        arrival: std::collections::VecDeque<i64>,
        evicted: u64,
    }

    impl LiveCache {
        fn insert(&mut self, id: i64, live: LiveInteraction) -> Option<LiveInteraction> {
            let replaced = self.remove(&id);
            while self.entries.len() >= LIVE_INTERACTION_CAP {
                let Some(oldest) = self.arrival.pop_front() else {
                    break;
                };
                if self.entries.remove(&oldest).is_some() {
                    self.evicted += 1;
                    tracing::warn!(
                    event = "live_interaction_evicted",
                    job_id = oldest,
                    cap = LIVE_INTERACTION_CAP,
                    evicted_total = self.evicted,
                    "live interaction cache full; evicted the oldest, its claim recovers from the durable job"
                );
                }
            }
            self.arrival.push_back(id);
            self.entries.insert(id, live);
            replaced
        }

        fn remove(&mut self, id: &i64) -> Option<LiveInteraction> {
            let live = self.entries.remove(id)?;
            if let Some(position) = self.arrival.iter().position(|queued| queued == id) {
                self.arrival.remove(position);
            }
            Some(live)
        }

        #[cfg(test)]
        fn len(&self) -> usize {
            self.entries.len()
        }

        #[cfg(test)]
        fn is_empty(&self) -> bool {
            self.entries.is_empty()
        }

        #[cfg(test)]
        fn contains_key(&self, id: &i64) -> bool {
            self.entries.contains_key(id)
        }
    }

    type LiveInteractions = Arc<std::sync::Mutex<LiveCache>>;

    fn commit(
        writer_live: &LiveInteractions,
        id: i64,
        interaction: Box<Interaction>,
        ticket: tokio::sync::oneshot::Receiver<bool>,
        observed_generation: u64,
    ) {
        writer_live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id,
                LiveInteraction {
                    interaction,
                    ticket,
                    generation: observed_generation,
                },
            );
    }

    struct Saved {
        id: i64,
        payload: String,
    }

    fn claim(
        live_interactions: &LiveInteractions,
        saved: &Saved,
    ) -> Result<(Option<LiveInteraction>, Option<OnboardingJob>), sqlx::Error> {
        let live = live_interactions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&saved.id);
        let job = if let Some(live) = &live {
            Some(OnboardingJob::Interaction(live.interaction.clone()))
        } else {
            OnboardingJob::recover(&saved.payload).map_err(|_| {
                sqlx::Error::InvalidArgument("invalid durable onboarding job".into())
            })?
        };

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
