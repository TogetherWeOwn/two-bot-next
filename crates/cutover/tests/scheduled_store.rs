//! Scheduled-message store integration tests (TOG-10081).
//!
//! Exercises `two-bot-core`'s `scheduled_store` (sqlx) against
//! `TWO_TEST_DATABASE_URL`: a pre-provisioned scratch database on agent-testdb
//! or the CI Postgres service container, never staging or production. Each
//! test owns a schema-only pool, so concurrent suites can reuse fixture ids.
//! Teardown is awaited before propagating scenario panics or query failures.
//!
//! Local runs without an explicit URL skip DB tests. CI requires the URL.
//! Once configured, all URL, connection, migration and cleanup errors fail
//! the suite — an unavailable database is never reported as a passing skip.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, QueryBuilder};

use two_bot_core::{
    audit_scheduled, claim_due, complete_run, delete_scheduled, format_iso_ms, get_scheduled,
    list_scheduled, put_scheduled, resolve_scheduled_id_store, retry_scheduled,
    ScheduledAuditInput, ScheduledWrite,
};
static SCHEMA_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    schema: String,
}

impl TestDb {
    async fn finish(self) {
        self.pool.close().await;
        // Only our generated schema, never shared guild rows or public tables.
        QueryBuilder::<Postgres>::new("DROP SCHEMA ")
            .push(&self.schema)
            .push(" CASCADE")
            .build()
            .execute(&self.admin)
            .await
            .expect("scheduled test schema cleanup failed");
        let remains: bool = sqlx::query_scalar("SELECT to_regnamespace($1) IS NOT NULL")
            .bind(&self.schema)
            .fetch_one(&self.admin)
            .await
            .unwrap();
        assert!(!remains, "scheduled test schema survived teardown");
        self.admin.close().await;
    }
}

/// Fixed clock: 2026-01-01T00:00:00.000Z, so every timestamp is exact.
const T0: u64 = 1_767_225_600_000;
const NOW: u64 = T0 + 600_000; // 00:10, the tick time
const LEASE: u64 = NOW + 60_000; // claim parks the row here

fn validate_test_db_url(url: &str, ci: bool) -> Result<(), &'static str> {
    let options: sqlx::postgres::PgConnectOptions =
        url.parse().map_err(|_| "invalid TWO_TEST_DATABASE_URL")?;
    let test_host = options.get_host() == "agent-testdb"
        || (ci && matches!(options.get_host(), "localhost" | "127.0.0.1"));
    if !test_host
        || options.get_username() != "agent_test"
        || !options
            .get_database()
            .is_some_and(|db| db.starts_with("two_bot_test_"))
    {
        return Err("refusing non-test database; use agent-testdb or the CI service container");
    }
    Ok(())
}

fn iso(ms: u64) -> String {
    format_iso_ms(ms)
}

fn write(guild: &str, id: &str, next_run_at: &str, interval: Option<i64>) -> ScheduledWrite {
    ScheduledWrite {
        id: id.to_owned(),
        guild_id: guild.to_owned(),
        channel_id: "chan9".to_owned(),
        body: "hello".to_owned(),
        next_run_at: next_run_at.to_owned(),
        interval_seconds: interval,
        enabled: true,
        created_by: "admin1".to_owned(),
        created_at: iso(T0),
        updated_by: "admin1".to_owned(),
        updated_at: iso(T0),
    }
}

async fn setup() -> Option<TestDb> {
    let ci = std::env::var("CI").is_ok_and(|value| value == "true");
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) if !ci => return None,
        Err(_) => panic!("TWO_TEST_DATABASE_URL must be configured for CI"),
    };
    validate_test_db_url(&url, ci).expect("unsafe or invalid test database configuration");
    let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options.clone())
        .await
        .expect("test database connection failed");
    let schema = format!(
        "sched_test_{}_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SCHEMA_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    // Identifier is only a constant prefix plus numeric process/time/sequence.
    QueryBuilder::<Postgres>::new("CREATE SCHEMA ")
        .push(&schema)
        .build()
        .execute(&admin)
        .await
        .expect("test schema creation failed");
    let path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |conn, _| {
            let path = path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(path)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect_lazy_with(options);
    Some(TestDb {
        admin,
        pool,
        schema,
    })
}

async fn migrate(pool: &PgPool) {
    for migration in [
        include_str!("../migrations/0140_scheduled_messages.sql"),
        include_str!("../migrations/0141_scheduled_messages_legacy_upgrade.sql"),
        include_str!("../migrations/0150_sticky_messages.sql"),
    ] {
        sqlx::raw_sql(migration)
            .execute(pool)
            .await
            .expect("scheduled test migration failed");
    }
}

async fn with_db<F, Fut>(scenario: F)
where
    F: FnOnce(PgPool) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let Some(db) = setup().await else {
        eprintln!("SKIP scheduled store scenario: no test database configured");
        return;
    };
    let pool = db.pool.clone();
    // Capture migration/query/assertion panics, then await teardown before failing.
    let result = tokio::spawn(async move {
        migrate(&pool).await;
        scenario(pool).await;
    })
    .await;
    db.finish().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error.into_panic());
    }
}

async fn rendezvous(barrier: &tokio::sync::Barrier) {
    tokio::time::timeout(Duration::from_secs(20), barrier.wait())
        .await
        .expect("isolated suite rendezvous timed out");
}

async fn isolated_claim_scenario(
    pool: PgPool,
    barrier: std::sync::Arc<tokio::sync::Barrier>,
    nonce: &'static str,
    remove: bool,
) {
    let guild = "same-guild";
    let id = "same-id";
    assert!(put_scheduled(&pool, &write(guild, id, &iso(T0), None))
        .await
        .unwrap());
    rendezvous(&barrier).await;
    let claimed = claim_due(&pool, guild, &iso(NOW), "same-claim", &iso(LEASE), nonce)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].occurrence_nonce.as_deref(), Some(nonce));
    rendezvous(&barrier).await;
    if remove {
        assert!(delete_scheduled(&pool, guild, id).await.unwrap());
    }
    rendezvous(&barrier).await;
    assert_eq!(
        get_scheduled(&pool, guild, id).await.unwrap().is_none(),
        remove
    );
}

#[tokio::test]
async fn concurrent_suites_keep_identical_ids_and_claims_isolated() {
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let other = barrier.clone();
    let (first, second) = tokio::join!(
        tokio::spawn(with_db(move |pool| isolated_claim_scenario(
            pool,
            barrier,
            "nonce-first",
            true
        ))),
        tokio::spawn(with_db(move |pool| isolated_claim_scenario(
            pool,
            other,
            "nonce-second",
            false
        ))),
    );
    first.unwrap();
    second.unwrap();
}

#[tokio::test]
async fn scenario_panic_cleans_up_before_propagating() {
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none()
        && std::env::var("CI").as_deref() != Ok("true")
    {
        return;
    }
    let result = tokio::spawn(with_db(|_| async { panic!("deliberate scenario panic") })).await;
    // with_db verifies the schema is gone before it rethrows the scenario panic.
    let panic = result.unwrap_err().into_panic();
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"deliberate scenario panic")
    );
}

#[test]
fn test_database_configuration_refuses_non_test_targets() {
    assert!(validate_test_db_url("invalid-url", false).is_err());
    assert!(
        validate_test_db_url("postgres://agent_test@production/two_bot_test_sched", true).is_err()
    );
    assert!(validate_test_db_url("postgres://agent_test@agent-testdb/two_bot", false).is_err());
    assert!(
        validate_test_db_url("postgres://other@agent-testdb/two_bot_test_sched", false).is_err()
    );
    assert!(
        validate_test_db_url("postgres://agent_test@localhost/two_bot_test_sched", false).is_err()
    );
    assert!(validate_test_db_url(
        "postgres://agent_test@agent-testdb/two_bot_test_sched",
        false
    )
    .is_ok());
    assert!(
        validate_test_db_url("postgres://agent_test@127.0.0.1/two_bot_test_sched", true).is_ok()
    );
}

#[tokio::test]
async fn legacy_queue_upgrade_preserves_rows_and_decodes_store_shape() {
    with_db(|pool| async move {
        let pool = &pool;
        for has_claim_columns in [false, true] {
            // Replace only this test schema's tables, then roll back each legacy shape.
            let mut tx = pool.begin().await.unwrap();
            sqlx::raw_sql(
                "DROP TABLE scheduled_messages; DROP TABLE automation_audit_log;",
            )
            .execute(&mut *tx)
            .await
            .unwrap();
            sqlx::raw_sql(include_str!("fixtures/scheduled_messages_legacy.sql"))
                .execute(&mut *tx)
                .await
                .unwrap();
            if has_claim_columns {
                sqlx::raw_sql(
                    "ALTER TABLE scheduled_messages ADD COLUMN claim_token TEXT, ADD COLUMN claimed_at TEXT;
                     UPDATE scheduled_messages SET claim_token = 'claim-before-upgrade',
                         claimed_at = '2026-01-01T00:00:00.000Z' WHERE id = 'legacy-hourly';",
                )
                .execute(&mut *tx)
                .await
                .unwrap();
            }
            let before: Vec<(String,)> = sqlx::query_as(
                "SELECT (to_jsonb(s) - 'claim_token' - 'claimed_at' - 'occurrence_nonce')::TEXT
                 FROM scheduled_messages s ORDER BY id",
            )
            .fetch_all(&mut *tx)
            .await
            .unwrap();
            for _ in 0..2 {
                sqlx::raw_sql(include_str!("../migrations/0140_scheduled_messages.sql"))
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                sqlx::raw_sql(include_str!(
                    "../migrations/0141_scheduled_messages_legacy_upgrade.sql"
                ))
                .execute(&mut *tx)
                .await
                .unwrap();
            }
            let after: Vec<(String,)> = sqlx::query_as(
                "SELECT (to_jsonb(s) - 'claim_token' - 'claimed_at' - 'occurrence_nonce')::TEXT
                 FROM scheduled_messages s ORDER BY id",
            )
            .fetch_all(&mut *tx)
            .await
            .unwrap();
            assert_eq!(before, after, "all legacy definition and run facts survive");
            let rows: Vec<two_bot_core::ScheduledMessageRow> =
                sqlx::query_as("SELECT * FROM scheduled_messages ORDER BY id")
                    .fetch_all(&mut *tx)
                    .await
                    .expect("legacy rows must decode into the sqlx store shape");
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].interval_seconds, Some(3600));
            assert_eq!(rows[1].interval_seconds, None);
            assert!(rows.iter().all(|row| row.occurrence_nonce.is_none()));
            assert_eq!(
                rows[0].claim_token.as_deref(),
                has_claim_columns.then_some("claim-before-upgrade")
            );
            assert_eq!(
                rows[0].claimed_at.as_deref(),
                has_claim_columns.then_some("2026-01-01T00:00:00.000Z")
            );
            for invalid in [
                "UPDATE scheduled_messages SET interval_seconds = 59 WHERE id = 'legacy-hourly'",
                "UPDATE scheduled_messages SET interval_seconds = 31536001 WHERE id = 'legacy-hourly'",
                "UPDATE scheduled_messages SET body = '' WHERE id = 'legacy-hourly'",
            ] {
                sqlx::query("SAVEPOINT invalid_legacy_write")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                let error = sqlx::query(invalid)
                .execute(&mut *tx)
                .await
                .expect_err("legacy CHECK constraints must remain enforced");
                assert_eq!(
                    error.as_database_error().unwrap().code().as_deref(),
                    Some("23514")
                );
                sqlx::query("ROLLBACK TO SAVEPOINT invalid_legacy_write")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
            tx.rollback().await.unwrap();
        }
    })
    .await;
}

#[tokio::test]
async fn put_get_list_round_trip_in_ticker_order() {
    with_db(|pool| async move {
        let pool = &pool;
        let guild = "sched-list";

        assert!(
            put_scheduled(pool, &write(guild, "late", &iso(T0 + 3_600_000), None))
                .await
                .unwrap()
        );
        assert!(put_scheduled(pool, &write(guild, "early", &iso(T0), None))
            .await
            .unwrap());

        let row = get_scheduled(pool, guild, "early").await.unwrap().unwrap();
        assert_eq!(row.body, "hello");
        assert_eq!(row.next_run_at, iso(T0));
        assert!(row.enabled);
        assert!(row.claim_token.is_none());

        // Ticker order: earliest next_run_at first.
        let rows = list_scheduled(pool, guild).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "early");
        assert_eq!(rows[1].id, "late");

        // Prefix resolution: unique, missing, ambiguous.
        assert_eq!(
            resolve_scheduled_id_store(pool, guild, "ear")
                .await
                .unwrap(),
            Some("early".to_owned())
        );
        assert_eq!(
            resolve_scheduled_id_store(pool, guild, "nope")
                .await
                .unwrap(),
            None
        );
        assert!(put_scheduled(pool, &write(guild, "early2", &iso(T0), None))
            .await
            .unwrap());
        assert_eq!(
            resolve_scheduled_id_store(pool, guild, "ear")
                .await
                .unwrap(),
            None,
            "ambiguous prefix refuses"
        );

        assert!(delete_scheduled(pool, guild, "early").await.unwrap());
        assert!(!delete_scheduled(pool, guild, "early").await.unwrap());
        assert!(get_scheduled(pool, guild, "early").await.unwrap().is_none());
    })
    .await;
}

#[tokio::test]
async fn prefixes_treat_like_metacharacters_literally() {
    with_db(|pool| async move {
        let pool = &pool;
        let guild = "sched-literal-prefix";
        assert!(put_scheduled(pool, &write(guild, "abc123", &iso(T0), None))
            .await
            .unwrap());
        for prefix in ["%", "_", "a_c", "a%", "abc\\", "ABC"] {
            assert_eq!(
                resolve_scheduled_id_store(pool, guild, prefix)
                    .await
                    .unwrap(),
                None
            );
        }

        let ids = ["abc123", "%literal", "_literal", "\\literal", "éclair"];
        for id in &ids[1..] {
            assert!(put_scheduled(pool, &write(guild, id, &iso(T0), None))
                .await
                .unwrap());
        }
        for prefix in ["%", "_", "\\", "é", "", "abc123"] {
            let expected = match two_bot_core::resolve_scheduled_id(ids.iter().copied(), prefix) {
                two_bot_core::IdResolution::Unique(id) => Some(id.to_owned()),
                _ => None,
            };
            assert_eq!(
                resolve_scheduled_id_store(pool, guild, prefix)
                    .await
                    .unwrap(),
                expected
            );
        }
        assert!(
            put_scheduled(pool, &write(guild, "%longer", &iso(T0), None))
                .await
                .unwrap()
        );
        assert_eq!(
            resolve_scheduled_id_store(pool, guild, "%").await.unwrap(),
            None
        );
    })
    .await;
}

#[tokio::test]
async fn two_due_messages_claim_independently_and_retry_keeps_nonce() {
    with_db(|pool| async move {
        let pool = &pool;
        let guild = "sched-two-due";
        for (id, body) in [("due-a", "first body"), ("due-b", "second body")] {
            let mut row = write(guild, id, &iso(T0), None);
            row.body = body.to_owned();
            assert!(put_scheduled(pool, &row).await.unwrap());
        }

        let now = iso(NOW);
        let lease = iso(LEASE);
        let (first, second) = tokio::join!(
            claim_due(pool, guild, &now, "claim-a", &lease, "nonce-a"),
            claim_due(pool, guild, &now, "claim-b", &lease, "nonce-b"),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].id, second[0].id);
        assert_eq!(first[0].channel_id, second[0].channel_id);
        assert_ne!(first[0].body, second[0].body);
        assert_eq!(first[0].occurrence_nonce.as_deref(), Some("nonce-a"));
        assert_eq!(second[0].occurrence_nonce.as_deref(), Some("nonce-b"));
        assert!(
            claim_due(pool, guild, &iso(NOW), "claim-c", &iso(LEASE), "nonce-c")
                .await
                .unwrap()
                .is_empty()
        );

        // An expired lease after restart still identifies the same occurrence.
        let restarted = claim_due(
            pool,
            guild,
            &iso(LEASE),
            "claim-restart",
            &iso(LEASE + 60_000),
            "nonce-new",
        )
        .await
        .unwrap();
        assert_eq!(restarted.len(), 1);
        let previous = [&first[0], &second[0]]
            .into_iter()
            .find(|row| row.id == restarted[0].id)
            .unwrap();
        assert_eq!(restarted[0].occurrence_nonce, previous.occurrence_nonce);
    })
    .await;
}

#[tokio::test]
async fn single_due_row_has_exactly_one_winner_under_contention() {
    with_db(|pool| async move {
        let pool = &pool;
        let guild = "sched-claim-race";

        // A guild with no rows claims nothing.
        assert!(
            claim_due(
                pool,
                "sched-claim-race-empty",
                &iso(NOW),
                "claim-empty",
                &iso(LEASE),
                "nonce-empty"
            )
            .await
            .unwrap()
            .is_empty(),
            "empty queue returns none"
        );

        // One due row plus a future-dated row that must stay untouched (it is
        // not due at the tick time, so the race loser must find nothing).
        assert!(put_scheduled(pool, &write(guild, "race", &iso(T0), None))
            .await
            .unwrap());
        assert!(
            put_scheduled(pool, &write(guild, "later", &iso(NOW + 3_600_000), None))
                .await
                .unwrap()
        );

        // Two schedulers race for the queue: exactly one wins, and the winner
        // takes the earliest-due row.
        let now = iso(NOW);
        let lease = iso(LEASE);
        let (first, second) = tokio::join!(
            claim_due(pool, guild, &now, "claim-race-a", &lease, "nonce-race-a"),
            claim_due(pool, guild, &now, "claim-race-b", &lease, "nonce-race-b"),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(
            first.len() + second.len(),
            1,
            "single due row claimed exactly once under contention"
        );
        let winner = first.first().or(second.first()).unwrap();
        assert_eq!(winner.id, "race", "earliest-due row wins the race");
        assert_eq!(
            winner.next_run_at, lease,
            "claim parks the row at the lease"
        );
        let (token, nonce) = if first.len() == 1 {
            ("claim-race-a", "nonce-race-a")
        } else {
            ("claim-race-b", "nonce-race-b")
        };
        assert_eq!(winner.claim_token.as_deref(), Some(token));
        assert_eq!(winner.occurrence_nonce.as_deref(), Some(nonce));

        // The loser (and any latecomer) finds nothing due: no double-fire.
        assert!(
            claim_due(
                pool,
                guild,
                &iso(NOW),
                "claim-race-c",
                &iso(LEASE),
                "nonce-race-c"
            )
            .await
            .unwrap()
            .is_empty(),
            "claimed row must not be re-claimed"
        );
        let later = get_scheduled(pool, guild, "later").await.unwrap().unwrap();
        assert!(later.claim_token.is_none(), "later-due row stays unclaimed");
    })
    .await;
}

#[tokio::test]
async fn claim_happens_once_then_one_shot_disables() {
    with_db(|pool| async move {
        let pool = &pool;
        let guild = "sched-claim-once";

        // Due row (next_run_at in the past relative to the tick).
        assert!(put_scheduled(pool, &write(guild, "once", &iso(T0), None))
            .await
            .unwrap());
        // Future row is never claimed.
        assert!(
            put_scheduled(pool, &write(guild, "future", &iso(NOW + 3_600_000), None))
                .await
                .unwrap()
        );

        let claimed = claim_due(pool, guild, &iso(NOW), "claim-1", &iso(LEASE), "nonce-1")
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, "once");
        assert_eq!(claimed[0].claim_token.as_deref(), Some("claim-1"));
        assert_eq!(claimed[0].occurrence_nonce.as_deref(), Some("nonce-1"));
        // The lease parks next_run_at in the future: a restarted scheduler that
        // missed the claim columns still sees the row as not-due.
        assert_eq!(claimed[0].next_run_at, iso(LEASE));

        // A second scheduler (or a restart) claims nothing: no double post.
        let reclaimed = claim_due(pool, guild, &iso(NOW), "claim-2", &iso(LEASE), "nonce-2")
            .await
            .unwrap();
        assert!(reclaimed.is_empty(), "claimed row must not be re-claimed");

        // Completing the run disables the one-shot and clears the claim.
        let done = complete_run(pool, guild, "once", &iso(NOW), Some("msg-1"), "claim-1")
            .await
            .unwrap()
            .unwrap();
        assert!(!done.enabled);
        assert_eq!(done.last_run_at.as_deref(), Some(iso(NOW).as_str()));
        assert_eq!(done.last_message_id.as_deref(), Some("msg-1"));
        assert!(done.claim_token.is_none());

        // Disabled rows never come due again.
        let again = claim_due(
            pool,
            guild,
            &iso(NOW + 3_600_000),
            "claim-3",
            &iso(LEASE),
            "n",
        )
        .await
        .unwrap();
        assert!(again.iter().all(|r| r.id != "once"));
    })
    .await;
}

#[tokio::test]
async fn stale_completion_loses_and_recurring_advances() {
    with_db(|pool| async move {
        let pool = &pool;
        let guild = "sched-stale-recur";

        // Stale path: claim, then the definition is replaced (clears the claim),
        // then the old completion arrives and loses.
        assert!(put_scheduled(pool, &write(guild, "edited", &iso(T0), None))
            .await
            .unwrap());
        assert_eq!(
            claim_due(
                pool,
                guild,
                &iso(NOW),
                "claim-old",
                &iso(LEASE),
                "nonce-old"
            )
            .await
            .unwrap()
            .len(),
            1
        );
        let mut replaced = write(guild, "edited", &iso(T0 + 5_000), None);
        replaced.created_by = "admin1".to_owned();
        replaced.created_at = iso(T0);
        assert!(put_scheduled(pool, &replaced).await.unwrap());
        assert!(
            complete_run(pool, guild, "edited", &iso(NOW), Some("msg-x"), "claim-old")
                .await
                .unwrap()
                .is_none(),
            "completion with a cleared claim must lose"
        );

        // Recurring path: advance from the run time, not the stale schedule.
        assert!(
            put_scheduled(pool, &write(guild, "hourly", &iso(T0), Some(3600)))
                .await
                .unwrap()
        );
        let claimed = claim_due(pool, guild, &iso(NOW), "claim-h", &iso(LEASE), "nonce-h")
            .await
            .unwrap();
        // "hourly" is due before "edited" (T0+5s); claim only that occurrence.
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, "hourly");
        let done = complete_run(pool, guild, "hourly", &iso(NOW), Some("msg-h"), "claim-h")
            .await
            .unwrap()
            .unwrap();
        assert!(done.enabled, "recurring rows stay enabled");
        assert_eq!(
            done.next_run_at,
            iso(NOW + 3_600_000),
            "advance from run time: no catch-up burst"
        );
        assert!(done.claim_token.is_none());
        assert!(done.occurrence_nonce.is_none());
    })
    .await;
}

#[tokio::test]
async fn retry_requeues_keeping_nonce() {
    with_db(|pool| async move {
        let pool = &pool;
        let guild = "sched-retry";

        assert!(
            put_scheduled(pool, &write(guild, "flaky", &iso(T0), Some(3600)))
                .await
                .unwrap()
        );
        let claimed = claim_due(pool, guild, &iso(NOW), "claim-f", &iso(LEASE), "nonce-f")
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);

        // Retryable failure: re-queue 30 s out, keeping the nonce.
        let retry_at = iso(NOW + 30_000);
        assert!(
            retry_scheduled(pool, guild, "flaky", "claim-f", &retry_at, true)
                .await
                .unwrap()
        );
        let row = get_scheduled(pool, guild, "flaky").await.unwrap().unwrap();
        assert_eq!(row.next_run_at, retry_at);
        assert!(row.claim_token.is_none());
        assert_eq!(row.occurrence_nonce.as_deref(), Some("nonce-f"));

        // Wrong claim token rewrites nothing.
        assert!(
            !retry_scheduled(pool, guild, "flaky", "claim-wrong", &iso(NOW), true)
                .await
                .unwrap()
        );
        // Orphan-cleanup path drops the nonce.
        let claimed = claim_due(
            pool,
            guild,
            &iso(NOW + 60_000),
            "claim-f2",
            &iso(LEASE),
            "n2",
        )
        .await
        .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(
            claimed[0].occurrence_nonce.as_deref(),
            Some("nonce-f"),
            "existing nonce survives a fresh claim"
        );
        assert!(
            retry_scheduled(pool, guild, "flaky", "claim-f2", &iso(NOW + 60_000), false)
                .await
                .unwrap()
        );
        let row = get_scheduled(pool, guild, "flaky").await.unwrap().unwrap();
        assert!(row.occurrence_nonce.is_none());
    })
    .await;
}

#[tokio::test]
async fn schedule_ids_are_guild_scoped() {
    with_db(|pool| async move {
        let pool = &pool;
        let (a, b) = ("sched-scope-a", "sched-scope-b");

        assert!(put_scheduled(pool, &write(a, "shared", &iso(T0), None))
            .await
            .unwrap());
        // Same id in another guild is refused (legacy guild-scoped upsert).
        assert!(!put_scheduled(pool, &write(b, "shared", &iso(T0), None))
            .await
            .unwrap());
        assert!(get_scheduled(pool, b, "shared").await.unwrap().is_none());
        // The refused upsert left no row in B, so deleting there removes nothing —
        // while A's row is untouched.
        assert!(!delete_scheduled(pool, b, "shared").await.unwrap());
        assert!(get_scheduled(pool, a, "shared").await.unwrap().is_some());
        assert!(delete_scheduled(pool, a, "shared").await.unwrap());
    })
    .await;
}

#[tokio::test]
async fn audit_rows_record_outcomes() {
    with_db(|pool| async move {
        let pool = &pool;
        let guild = "sched-audit";

        audit_scheduled(
            pool,
            &ScheduledAuditInput {
                guild_id: guild.to_owned(),
                actor_id: None,
                action: "scheduled.run".to_owned(),
                target_key: Some("once".to_owned()),
                outcome: "ok".to_owned(),
                reason: None,
            },
            &iso(NOW),
            "audit-1",
        )
        .await
        .unwrap();

        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM automation_audit_log
             WHERE guild_id = $1 AND created_at = $2::TEXT::TIMESTAMPTZ",
        )
        .bind(guild)
        .bind(iso(NOW))
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
    })
    .await;
}
