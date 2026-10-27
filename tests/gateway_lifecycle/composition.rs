// Protected live-source composition template. The actual voice factory spawns
// the actual collector/persist loop; actual main retains and joins its handle.
// Only configuration/transport/runtime-construction leaves and clocks are
// doubled. This is not a full actor-registry or Discord/SQL integration test.
// Guard Drop is live source, owned by the controlled gateway producer; its sink
// closes the real collector's open spans at the shared fixture wall clock.
// Main's Instant is a Tokio clock substitution, so paused time consumes its
// single absolute shutdown budget. No coordinator is implemented in this file.

mod settings_jobs {
    pub fn live() -> Option<two_bot_core::settings::LiveSettings> {
        None
    }
}

mod composition_adapter {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::RwLock;
    use tokio::time::Instant;
    use tracing::{info, warn};
    use twilight_cache_inmemory::DefaultInMemoryCache;
    use twilight_gateway::Event;
    use twilight_model::gateway::payload::incoming::PresenceUpdate;
    use two_bot::voice_rooms::VoiceEventSink;
    use two_bot_core::settings::LiveSettings;
    use two_bot_core::{AutomodPolicy, Config, Secret};
    use two_bot_cutover::DB_POOL_MAX_DEFAULT;

    struct VoiceGates {
        enabled: bool,
    }

    impl VoiceGates {
        fn from_env() -> Self {
            Self { enabled: true }
        }
    }

    enum GatewayState {
        Armed,
        Draining,
    }

    #[derive(Default)]
    struct FakeInner {
        disconnect_calls: std::sync::Mutex<u32>,
        disconnect_from_drop_calls: std::sync::Mutex<u32>,
    }

    struct FakeRuntime {
        collector: Arc<crate::persistence_adapter::GameActivityCollector>,
        inner: Arc<FakeInner>,
    }

    impl FakeRuntime {
        fn runtime(&self) -> &Self {
            self
        }

        fn game_activity_collector(
            &self,
        ) -> &Arc<crate::persistence_adapter::GameActivityCollector> {
            &self.collector
        }
    }

    impl VoiceEventSink for FakeRuntime {
        fn handle(&self, _event: &Event, _cache: &DefaultInMemoryCache) {}
        fn presence(&self, _update: &PresenceUpdate) {}
        fn needs_bootstrap(&self, _cache: &DefaultInMemoryCache) -> bool {
            false
        }
        fn disconnect(&self) {
            *self.inner.disconnect_calls.lock().expect("disconnect lock") += 1;
        }
        fn disconnect_from_drop(&self) {
            *self
                .inner
                .disconnect_from_drop_calls
                .lock()
                .expect("drop lock") += 1;
            self.collector
                .close_all_opens(crate::persistence_adapter::now_ms());
        }
    }

    #[derive(Debug)]
    struct SetupError;

    impl std::fmt::Display for SetupError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("synthetic voice setup refused")
        }
    }

    impl std::error::Error for SetupError {}

    struct FakeVoiceDb {
        pool: crate::persistence_adapter::Database,
    }

    impl FakeVoiceDb {
        fn pool(&self) -> &crate::persistence_adapter::Database {
            &self.pool
        }
    }

    async fn connect_voice(_url: &str, _max: u32, _skip: bool) -> Result<FakeVoiceDb, SetupError> {
        Ok(FakeVoiceDb {
            pool: crate::persistence_adapter::Database::default(),
        })
    }

    fn build_production_runtime(
        _token: &str,
        pool: crate::persistence_adapter::Database,
        _name_policy: AutomodPolicy,
        live_source: impl Fn() -> Option<LiveSettings> + Send + Sync + 'static,
    ) -> Result<FakeRuntime, SetupError> {
        let _ = live_source();
        let collector = Arc::new(crate::persistence_adapter::GameActivityCollector::new());
        let inner = Arc::new(FakeInner::default());
        test_hooks::stash(VoiceIo {
            pool,
            collector: Arc::clone(&collector),
            inner: Arc::clone(&inner),
        });
        Ok(FakeRuntime { collector, inner })
    }

    struct VoiceIo {
        pool: crate::persistence_adapter::Database,
        collector: Arc<crate::persistence_adapter::GameActivityCollector>,
        inner: Arc<FakeInner>,
    }

    mod test_hooks {
        use std::sync::{Mutex, OnceLock};
        static SLOT: OnceLock<Mutex<Option<super::VoiceIo>>> = OnceLock::new();
        fn slot() -> &'static Mutex<Option<super::VoiceIo>> {
            SLOT.get_or_init(|| Mutex::new(None))
        }
        pub fn stash(io: super::VoiceIo) {
            *slot().lock().expect("voice io lock") = Some(io);
        }
        pub fn take() -> Option<super::VoiceIo> {
            slot().lock().expect("voice io lock").take()
        }
    }

    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    // LIVE_BUILD_VOICE_RUNTIME
    // LIVE_VOICE_GUARD
    // LIVE_MAIN_HELPERS

    mod shutdown {
        use std::time::Duration;
        // LIVE_SHUTDOWN_DEADLINE
    }
    mod eventsub_reconcile {
        use std::time::Duration;
        use two_bot_core::twitch_helix::HELIX_TIMEOUT_MS;
        // LIVE_EVENTSUB_JOIN
    }
    mod server {
        // LIVE_SHUTDOWN_REQUESTED
    }

    struct LiveBoot {
        shutdown: tokio::sync::watch::Sender<bool>,
        voice: Option<Arc<dyn VoiceEventSink>>,
        worker_supervisors: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
        latch_mark: Arc<std::sync::Mutex<Option<Instant>>>,
        latch_observer: tokio::task::JoinHandle<()>,
    }

    async fn live_boot() -> LiveBoot {
        crate::persistence_adapter::reset_clock();
        let config = Config {
            discord_token: Some(Secret::new("synthetic-token".to_owned())),
            database_url: Some(Secret::new("synthetic-database".to_owned())),
            listen_addr: "127.0.0.1:0".to_owned(),
            guild_id: Some(22),
        };
        // LIVE_BOOT_VOICE
        // LIVE_BOOT_RETENTION
        let _ = &stopping;
        LiveBoot {
            shutdown,
            voice,
            worker_supervisors,
            latch_mark,
            latch_observer,
        }
    }

    async fn live_service(
        boot: LiveBoot,
        gateway_task: Option<tokio::task::JoinHandle<Result<(), sqlx::Error>>>,
        http: impl std::future::Future<Output = std::io::Result<()>>,
        gateway: Arc<RwLock<GatewayState>>,
    ) -> std::io::Result<()> {
        let LiveBoot {
            shutdown,
            voice,
            worker_supervisors,
            latch_mark,
            latch_observer,
        } = boot;
        let _ = &voice;
        let reconcile_task: Option<tokio::task::JoinHandle<()>> = None;
        // LIVE_SERVICE
        result
    }

    const GUILD: u64 = 111;
    const MEMBER: u64 = 222;
    const MEMBER_GUARD: u64 = 333;

    async fn settle() {
        crate::persistence_adapter::settle().await;
    }

    async fn completed(task: tokio::task::JoinHandle<std::io::Result<()>>, reason: &str) {
        let finished = task.is_finished();
        if !finished {
            task.abort();
            let result = task.await;
            assert!(
                result.is_err_and(|error| error.is_cancelled()),
                "fixture abort must join"
            );
            assert!(finished, "{reason}");
            return;
        }
        let result = task.await;
        assert!(result.is_ok(), "service task must not panic");
        assert!(
            result.ok().is_some_and(|value| value.is_ok()),
            "service failed after release"
        );
    }

    fn assert_guard_closed(io: &VoiceIo) {
        assert_eq!(
            *io.inner
                .disconnect_from_drop_calls
                .lock()
                .expect("drop lock"),
            1
        );
        assert_eq!(
            *io.inner.disconnect_calls.lock().expect("disconnect lock"),
            0
        );
    }

    // Break caught: main drops the handle returned by the actual voice factory.
    // Gateway and HTTP are both DONE before the pending-service assertion.
    #[tokio::test(start_paused = true)]
    async fn service_retains_live_flush_handle_until_trailing_persist_releases() {
        let _serial = SERIAL.lock().await;
        let boot = live_boot().await;
        let io = test_hooks::take().expect("voice enabled");
        let start = crate::persistence_adapter::T0;
        crate::persistence_adapter::episode(&io.collector, GUILD, MEMBER, start);
        crate::persistence_adapter::playing(&io.collector, GUILD, MEMBER_GUARD, start);
        crate::persistence_adapter::stop_playing(
            &io.collector,
            GUILD,
            MEMBER_GUARD,
            start + 120_000,
        );
        let (entered, release) = io.pool.hold_next_commit();
        tokio::time::advance(Duration::from_secs(180)).await;
        let guard = VoiceConnectionGuard(boot.voice.clone());
        let (gateway_done, gateway_finished) = tokio::sync::oneshot::channel();
        let receiver = boot.shutdown.subscribe();
        let gateway_task = Some(tokio::spawn(async move {
            server::shutdown_requested(receiver).await;
            drop(guard);
            let _ = gateway_done.send(());
            Ok::<(), sqlx::Error>(())
        }));
        let (http_done, http_finished) = tokio::sync::oneshot::channel();
        let http = async move {
            let _ = http_done.send(());
            Ok::<(), std::io::Error>(())
        };
        let task = tokio::spawn(live_service(
            boot,
            gateway_task,
            http,
            Arc::new(RwLock::new(GatewayState::Armed)),
        ));
        settle().await;
        assert!(gateway_finished.await.is_ok());
        assert!(http_finished.await.is_ok());
        assert!(entered.await.is_ok());
        assert!(
            !task.is_finished(),
            "service completed while its actual trailing persist was held"
        );
        assert_eq!(io.pool.totals(), (0, 0, 0));
        assert_guard_closed(&io);
        assert!(
            release.send(()).is_ok(),
            "actual flush was cancelled before release"
        );
        settle().await;
        completed(task, "service did not join its actual flush after release").await;
        assert_eq!(
            io.pool.totals(),
            (240_000, 240_000, 2),
            "both intervals must persist once"
        );
    }

    // Break caught: the shared stop latch starts the sole final snapshot before
    // accepted producer work and the real gateway guard's final disconnect.
    #[tokio::test(start_paused = true)]
    async fn accepted_producer_commit_orders_before_trailing_flush() {
        let _serial = SERIAL.lock().await;
        let boot = live_boot().await;
        let io = test_hooks::take().expect("voice enabled");
        let first = crate::persistence_adapter::T0;
        crate::persistence_adapter::episode(&io.collector, GUILD, MEMBER, first);
        settle().await;
        tokio::time::advance(Duration::from_secs(3600)).await;
        settle().await;
        assert_eq!(
            io.pool.totals(),
            (120_000, 120_000, 1),
            "ordinary hourly flush changed"
        );
        let second = first + 3_600_000;
        tokio::time::advance(Duration::from_secs(180)).await;
        let collector = Arc::clone(&io.collector);
        let guard = VoiceConnectionGuard(boot.voice.clone());
        let (release, held) = tokio::sync::oneshot::channel();
        let gateway_task = Some(tokio::spawn(async move {
            let connection = guard;
            assert!(held.await.is_ok(), "accepted producer must be released");
            crate::persistence_adapter::episode(&collector, GUILD, MEMBER, second);
            crate::persistence_adapter::playing(&collector, GUILD, MEMBER_GUARD, second);
            crate::persistence_adapter::stop_playing(
                &collector,
                GUILD,
                MEMBER_GUARD,
                second + 120_000,
            );
            drop(connection);
            Ok::<(), sqlx::Error>(())
        }));
        let stop = boot.shutdown.subscribe();
        let http = async { Ok::<(), std::io::Error>(()) };
        let task = tokio::spawn(live_service(
            boot,
            gateway_task,
            http,
            Arc::new(RwLock::new(GatewayState::Armed)),
        ));
        settle().await;
        assert!(
            *stop.borrow(),
            "real service supervision did not latch shutdown"
        );
        assert!(
            !task.is_finished(),
            "service finished before its accepted producer drained"
        );
        assert_eq!(io.pool.totals(), (120_000, 120_000, 1));
        assert!(release.send(()).is_ok(), "accepted producer was cancelled");
        settle().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        // This stays below the next 300s tick: a periodic flush cannot hide a
        // missing final write or make a discarded flush handle look joined.
        completed(
            task,
            "service did not finish after producer and disconnect drained",
        )
        .await;
        assert_guard_closed(&io);
        assert_eq!(
            io.pool.totals(),
            (360_000, 360_000, 3),
            "accepted writer and disconnect intervals must persist exactly once before completion"
        );
    }

    // Break caught: worker join starts a fresh budget after gateway drain.
    #[tokio::test(start_paused = true)]
    async fn actual_flush_uses_only_the_service_deadline_remaining_after_producer_drain() {
        let _serial = SERIAL.lock().await;
        let boot = live_boot().await;
        let io = test_hooks::take().expect("voice enabled");
        crate::persistence_adapter::episode(
            &io.collector,
            GUILD,
            MEMBER,
            crate::persistence_adapter::T0,
        );
        tokio::time::advance(Duration::from_secs(180)).await;
        let (entered, release) = io.pool.hold_next_commit();
        let budget = shutdown::deadline();
        let producer_time = budget.saturating_sub(Duration::from_secs(5));
        assert!(
            producer_time > Duration::ZERO,
            "fixture requires a nontrivial service budget"
        );
        let receiver = boot.shutdown.subscribe();
        let gateway_task = Some(tokio::spawn(async move {
            server::shutdown_requested(receiver).await;
            tokio::time::sleep(producer_time).await;
            Ok::<(), sqlx::Error>(())
        }));
        let task = tokio::spawn(live_service(
            boot,
            gateway_task,
            async { Ok(()) },
            Arc::new(RwLock::new(GatewayState::Armed)),
        ));
        settle().await;
        tokio::time::advance(producer_time).await;
        settle().await;
        assert!(entered.await.is_ok());
        assert!(
            !task.is_finished(),
            "flush must still be held after producer drain"
        );
        tokio::time::advance(Duration::from_secs(6)).await;
        settle().await;
        completed(
            task,
            "flush join restarted the budget instead of enforcing the common deadline",
        )
        .await;
        assert!(
            release.send(()).is_err(),
            "expired actual flush must be aborted AND joined, not detached"
        );
        assert_eq!(
            io.pool.totals(),
            (0, 0, 0),
            "aborted held transaction must never publish rows"
        );
    }
}
