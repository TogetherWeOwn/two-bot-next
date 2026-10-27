// Protected offline persistence/failure template. The marker is replaced with
// the LIVE collector, carry planner, persist_guild and flush scheduler. Only
// transaction I/O and the wall clock below are doubles; no drain/requeue or
// shutdown/retry coordination is implemented by the fixture.
pub mod persistence_adapter {
    use std::cell::Cell;
    use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
    use std::ops::{Deref, DerefMut};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::{oneshot, watch};
    use two_bot_core::game_activity::{
        self, ActivitySnapshot, ActivityType as CoreActivityType, GuildGameAggregate,
        PresenceDelta, VoiceSpan,
    };
    use two_bot_core::game_activity_store::GameActivityStoreError;
    use two_bot_core::Snowflake;

    pub const T0: i64 = 1_750_000_000_000;
    thread_local! {
        static ORIGIN: Cell<Option<tokio::time::Instant>> = const { Cell::new(None) };
    }

    pub fn reset_clock() {
        ORIGIN.with(|origin| origin.set(Some(tokio::time::Instant::now())));
    }

    pub fn now_ms() -> i64 {
        ORIGIN.with(|origin| {
            T0 + i64::try_from(
                tokio::time::Instant::now()
                    .duration_since(origin.get().expect("fixture clock initialized"))
                    .as_millis(),
            )
            .expect("fixture elapsed time fits")
        })
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Stage {
        Begin,
        Lock,
        PresenceGate,
        OptOutGate,
        VoiceGate,
        Upsert,
        Commit,
    }

    struct CommitHold {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    }

    #[derive(Default)]
    struct State {
        rows: Vec<GuildGameAggregate>,
        failures: VecDeque<Stage>,
        always_fail: Option<Stage>,
        attempts: usize,
        disabled: bool,
        voice_off: bool,
        opted_out: BTreeSet<String>,
        hold: Option<CommitHold>,
    }

    #[derive(Clone, Default)]
    pub struct Database {
        state: Arc<Mutex<State>>,
    }

    impl Database {
        pub fn fail_once(&self, stage: Stage) {
            self.state
                .lock()
                .expect("fixture lock")
                .failures
                .push_back(stage);
        }

        pub fn fail_always(&self, stage: Stage) {
            self.state.lock().expect("fixture lock").always_fail = Some(stage);
        }

        pub fn clear_failures(&self) {
            let mut state = self.state.lock().expect("fixture lock");
            state.failures.clear();
            state.always_fail = None;
        }

        pub fn privacy(&self, disabled: bool, voice_off: bool, opted_out: &[&str]) {
            let mut state = self.state.lock().expect("fixture lock");
            state.disabled = disabled;
            state.voice_off = voice_off;
            state.opted_out = opted_out.iter().map(|key| (*key).to_owned()).collect();
        }

        pub fn attempts(&self) -> usize {
            self.state.lock().expect("fixture lock").attempts
        }

        pub fn totals(&self) -> (i64, i64, u64) {
            self.state
                .lock()
                .expect("fixture lock")
                .rows
                .iter()
                .fold((0, 0, 0), |sum, row| {
                    (
                        sum.0 + row.total_play_ms,
                        sum.1 + row.in_voice_ms,
                        sum.2 + row.session_count,
                    )
                })
        }

        pub fn hold_next_commit(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
            let (entered, notified) = oneshot::channel();
            let (release, held) = oneshot::channel();
            let previous = self
                .state
                .lock()
                .expect("fixture lock")
                .hold
                .replace(CommitHold {
                    entered,
                    release: held,
                });
            assert!(previous.is_none(), "only one fixture commit may be held");
            (notified, release)
        }

        fn gate(&self, stage: Stage) -> Result<(), GameActivityStoreError> {
            let mut state = self.state.lock().expect("fixture lock");
            let failed = state.always_fail == Some(stage) || state.failures.front() == Some(&stage);
            if state.failures.front() == Some(&stage) {
                state.failures.pop_front();
            }
            if failed {
                Err(GameActivityStoreError::Db(::sqlx::Error::PoolTimedOut))
            } else {
                Ok(())
            }
        }

        pub async fn begin(&self) -> Result<sqlx::Transaction, GameActivityStoreError> {
            // Real database acquisition is an async boundary. Yield so even a
            // broken tight retry remains observable by the paused-time driver.
            tokio::task::yield_now().await;
            self.state.lock().expect("fixture lock").attempts += 1;
            self.gate(Stage::Begin)?;
            Ok(sqlx::Transaction {
                connection: sqlx::Connection {
                    database: self.clone(),
                    locked: false,
                    staged: Vec::new(),
                },
            })
        }
    }

    pub mod sqlx {
        use super::*;
        pub type PgPool = Database;
        pub type Error = ::sqlx::Error;

        pub struct Connection {
            pub(super) database: Database,
            pub(super) locked: bool,
            pub(super) staged: Vec<GuildGameAggregate>,
        }

        pub struct Transaction {
            pub(super) connection: Connection,
        }

        impl Deref for Transaction {
            type Target = Connection;
            fn deref(&self) -> &Self::Target {
                &self.connection
            }
        }

        impl DerefMut for Transaction {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.connection
            }
        }

        impl Transaction {
            pub async fn rollback(self) -> Result<(), GameActivityStoreError> {
                Ok(())
            }

            pub async fn commit(mut self) -> Result<(), GameActivityStoreError> {
                let held = self
                    .database
                    .state
                    .lock()
                    .expect("fixture lock")
                    .hold
                    .take();
                if let Some(held) = held {
                    let _ = held.entered.send(());
                    assert!(held.release.await.is_ok(), "held commit must be released");
                }
                self.database.gate(Stage::Commit)?;
                let rows = std::mem::take(&mut self.staged);
                self.database
                    .state
                    .lock()
                    .expect("fixture lock")
                    .rows
                    .extend(rows);
                Ok(())
            }
        }

        pub struct Query<'a>(&'a str);

        pub fn query(text: &str) -> Query<'_> {
            Query(text)
        }

        impl Query<'_> {
            pub async fn execute(
                self,
                connection: &mut Connection,
            ) -> Result<(), GameActivityStoreError> {
                // Only the real persistence path's transaction lock is modeled.
                assert_eq!(self.0, "LOCK TABLE privacy_opt_outs IN ROW EXCLUSIVE MODE");
                connection.database.gate(Stage::Lock)?;
                connection.locked = true;
                Ok(())
            }
        }
    }

    pub mod game_activity_store {
        use super::*;
        // Privacy filtering itself remains the actual domain function.
        pub use two_bot_core::game_activity_store::filter_opted_out;

        pub async fn game_analytics_disabled_in(
            connection: &mut sqlx::Connection,
            _guild: &str,
        ) -> Result<bool, GameActivityStoreError> {
            assert!(
                connection.locked,
                "privacy must be read inside the locked transaction"
            );
            connection.database.gate(Stage::PresenceGate)?;
            Ok(connection
                .database
                .state
                .lock()
                .expect("fixture lock")
                .disabled)
        }

        pub async fn opted_out_user_keys_in(
            connection: &mut sqlx::Connection,
            _guild: &str,
        ) -> Result<BTreeSet<String>, GameActivityStoreError> {
            connection.database.gate(Stage::OptOutGate)?;
            Ok(connection
                .database
                .state
                .lock()
                .expect("fixture lock")
                .opted_out
                .clone())
        }

        pub async fn voice_analytics_disabled_in(
            connection: &mut sqlx::Connection,
            _guild: &str,
        ) -> Result<bool, GameActivityStoreError> {
            connection.database.gate(Stage::VoiceGate)?;
            Ok(connection
                .database
                .state
                .lock()
                .expect("fixture lock")
                .voice_off)
        }

        pub async fn upsert_guild_aggregates_in(
            connection: &mut sqlx::Connection,
            rows: &[GuildGameAggregate],
        ) -> Result<u64, GameActivityStoreError> {
            connection.database.gate(Stage::Upsert)?;
            connection.staged.extend_from_slice(rows);
            Ok(rows.len() as u64)
        }
    }

    // PERSISTENCE_SOURCE

    pub fn playing(
        collector: &GameActivityCollector,
        guild: Snowflake,
        member: Snowflake,
        at: i64,
    ) {
        collector.observe_join(guild, member, at);
        collector.observe_presence(
            guild,
            PresenceDelta {
                user_key: member.to_string(),
                observed_at_ms: at,
                activities: vec![ActivitySnapshot {
                    activity_type: CoreActivityType::Playing,
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
    }

    pub fn stop_playing(
        collector: &GameActivityCollector,
        guild: Snowflake,
        member: Snowflake,
        at: i64,
    ) {
        collector.observe_presence(
            guild,
            PresenceDelta {
                user_key: member.to_string(),
                observed_at_ms: at,
                activities: vec![],
            },
        );
    }

    pub fn episode(
        collector: &GameActivityCollector,
        guild: Snowflake,
        member: Snowflake,
        start: i64,
    ) {
        playing(collector, guild, member, start);
        stop_playing(collector, guild, member, start + 120_000);
        collector.observe_leave(guild, member, start + 120_000);
    }

    pub async fn settle() {
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }

    async fn final_result(
        database: Database,
        collector: Arc<GameActivityCollector>,
    ) -> Option<bool> {
        let (stop, receiver) = watch::channel(true);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(35);
        let task = tokio::spawn(launch(database, collector, receiver, deadline));
        for _ in 0..360 {
            settle().await;
            if task.is_finished() {
                let result = task.await;
                assert!(result.is_ok(), "flush task must not panic");
                return result.ok();
            }
            tokio::time::advance(Duration::from_millis(100)).await;
        }
        settle().await;
        drop(stop);
        if task.is_finished() {
            let result = task.await;
            assert!(result.is_ok(), "flush task must not panic");
            result.ok()
        } else {
            task.abort();
            let cancelled = task.await;
            assert!(
                cancelled.is_err_and(|error| error.is_cancelled()),
                "fixture abort must join"
            );
            None
        }
    }

    fn fixture() -> (Database, Arc<GameActivityCollector>) {
        reset_clock();
        let database = Database::default();
        let collector = Arc::new(GameActivityCollector::new());
        episode(&collector, 111, 222, T0);
        (database, collector)
    }

    // Break caught: the trailing flush ignores an acquisition/privacy error
    // before drain and returns cleanly with the entire interval still buffered.
    #[tokio::test(start_paused = true)]
    async fn shutdown_retries_each_failure_before_drain() {
        for stage in [
            Stage::Begin,
            Stage::Lock,
            Stage::PresenceGate,
            Stage::OptOutGate,
            Stage::VoiceGate,
        ] {
            let (database, collector) = fixture();
            tokio::time::advance(Duration::from_secs(180)).await;
            database.fail_once(stage);
            let result = final_result(database.clone(), Arc::clone(&collector)).await;
            assert_eq!(
                database.totals(),
                (120_000, 120_000, 1),
                "shutdown must retry intact buffers"
            );
            assert_eq!(
                result,
                Some(true),
                "recovered shutdown must finish inside the budget"
            );
            assert!(
                database.attempts() >= 2,
                "the failed persistence must actually be retried"
            );
            assert!(persist_guild(
                &database,
                &collector,
                111,
                GAME_ACTIVITY_GAP_TIMEOUT_MS,
                now_ms()
            )
            .await
            .is_ok());
            assert_eq!(
                database.totals(),
                (120_000, 120_000, 1),
                "successful retry must consume buffers once"
            );
        }
    }

    // Break caught: requeued bridged deltas/spans have no next periodic tick
    // on shutdown, or a retry credits the same drained interval twice.
    #[tokio::test(start_paused = true)]
    async fn shutdown_retries_requeued_upsert_and_commit_once() {
        for stage in [Stage::Upsert, Stage::Commit] {
            let (database, collector) = fixture();
            tokio::time::advance(Duration::from_secs(180)).await;
            database.fail_once(stage);
            let result = final_result(database.clone(), Arc::clone(&collector)).await;
            assert_eq!(
                database.totals(),
                (120_000, 120_000, 1),
                "requeued final interval was lost or duplicated"
            );
            assert_eq!(result, Some(true), "requeued retry did not finish cleanly");
            assert!(database.attempts() >= 2);
            assert!(persist_guild(
                &database,
                &collector,
                111,
                GAME_ACTIVITY_GAP_TIMEOUT_MS,
                now_ms()
            )
            .await
            .is_ok());
            assert_eq!(database.totals(), (120_000, 120_000, 1));
        }
    }

    // Break caught: exhausted final persistence reports unit/Ok completion,
    // or retries forever rather than returning an explicit bounded failure.
    #[tokio::test(start_paused = true)]
    async fn exhausted_shutdown_budget_cannot_report_clean_completion() {
        for stage in [Stage::PresenceGate, Stage::Upsert, Stage::Commit] {
            let (database, collector) = fixture();
            tokio::time::advance(Duration::from_secs(180)).await;
            database.fail_always(stage);
            let result = final_result(database.clone(), Arc::clone(&collector)).await;
            assert_eq!(
                result,
                Some(false),
                "exhausted shutdown must expose failure, not clean success or an unbounded wait"
            );
            assert!(database.attempts() >= 1);
            assert_eq!(
                database.totals(),
                (0, 0, 0),
                "failed transactions must not publish partial rows"
            );
            database.clear_failures();
            assert!(persist_guild(
                &database,
                &collector,
                111,
                GAME_ACTIVITY_GAP_TIMEOUT_MS,
                now_ms()
            )
            .await
            .is_ok());
            assert_eq!(
                database.totals(),
                (120_000, 120_000, 1),
                "the failed final drain must retain retryable data"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn control_gate_failure_keeps_buffers_until_safe_retry() {
        for stage in [
            Stage::Begin,
            Stage::Lock,
            Stage::PresenceGate,
            Stage::OptOutGate,
            Stage::VoiceGate,
        ] {
            let (database, collector) = fixture();
            database.fail_once(stage);
            assert!(persist_guild(
                &database,
                &collector,
                111,
                GAME_ACTIVITY_GAP_TIMEOUT_MS,
                T0 + 180_000
            )
            .await
            .is_err());
            assert_eq!(
                database.totals(),
                (0, 0, 0),
                "a failed privacy read must never write"
            );
            assert_eq!(
                collector.buffered_counts(111),
                (2, 1, 0),
                "early failure must preserve both input buffers"
            );
            assert!(persist_guild(
                &database,
                &collector,
                111,
                GAME_ACTIVITY_GAP_TIMEOUT_MS,
                T0 + 180_000
            )
            .await
            .is_ok());
            assert_eq!(
                database.totals(),
                (120_000, 120_000, 1),
                "gate failure must not consume buffers"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn control_retry_rechecks_opt_out_and_independent_voice_gate() {
        let (database, collector) = fixture();
        episode(&collector, 111, 333, T0);
        database.fail_once(Stage::OptOutGate);
        assert!(persist_guild(
            &database,
            &collector,
            111,
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
            T0 + 180_000
        )
        .await
        .is_err());
        database.privacy(false, true, &["222"]);
        assert!(persist_guild(
            &database,
            &collector,
            111,
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
            T0 + 180_000
        )
        .await
        .is_ok());
        assert_eq!(
            database.totals(),
            (120_000, 0, 1),
            "retry must suppress opted-out play and all voice, not fail open"
        );
        database.privacy(false, false, &[]);
        assert!(persist_guild(
            &database,
            &collector,
            111,
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
            T0 + 180_000
        )
        .await
        .is_ok());
        assert_eq!(
            database.totals(),
            (120_000, 0, 1),
            "re-enable must not recover suppressed buffers"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn control_disabled_guild_discards_buffers_and_carries() {
        let (database, collector) = fixture();
        database.privacy(true, false, &[]);
        assert!(persist_guild(
            &database,
            &collector,
            111,
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
            T0 + 180_000
        )
        .await
        .is_ok());
        assert_eq!(database.totals(), (0, 0, 0));
        database.privacy(false, false, &[]);
        assert!(persist_guild(
            &database,
            &collector,
            111,
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
            T0 + 240_000
        )
        .await
        .is_ok());
        assert_eq!(
            database.totals(),
            (0, 0, 0),
            "re-enable starts fresh, never replays disabled data"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn control_hourly_failed_write_retries_without_double_counting() {
        let (database, collector) = fixture();
        let mut last = HashMap::new();
        flush_due_guilds(
            &database,
            &collector,
            &mut last,
            T0,
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
        )
        .await;
        database.fail_once(Stage::Commit);
        flush_due_guilds(
            &database,
            &collector,
            &mut last,
            T0 + 3_600_000,
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
        )
        .await;
        assert_eq!(database.totals(), (0, 0, 0));
        flush_due_guilds(
            &database,
            &collector,
            &mut last,
            T0 + 3_600_000,
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
        )
        .await;
        assert_eq!(database.totals(), (120_000, 120_000, 1));
        flush_due_guilds(
            &database,
            &collector,
            &mut last,
            T0 + 3_600_000,
            GAME_ACTIVITY_GAP_TIMEOUT_MS,
        )
        .await;
        assert_eq!(
            database.totals(),
            (120_000, 120_000, 1),
            "hourly retry must drain once"
        );
    }
}
