//! Explicit Postgres acceptance; never uses DATABASE_URL or operational secrets.
//! cargo test -p two-bot-core --features db --test join_risk_store --locked -- --ignored
#![cfg(feature = "db")]

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, QueryBuilder};
use tokio::sync::Barrier;
use two_bot_core::join_risk_store::{JoinRiskClaim, JoinRiskStore};
use two_bot_core::raid::{JoinRiskInput, JoinRiskObservation, JoinRiskPolicy};

type TestResult = Result<(), Box<dyn std::error::Error>>;
const MIGRATION: &str = include_str!("../../cutover/migrations/0360_join_risk_flags.sql");
const LEGACY_0015: &str =
    include_str!("../../cutover/tests/fixtures/legacy_migrations/0015_anti_nuke_containment.sql");
/// 2026-08-01T10:00:00.000Z.
const NOW: i64 = 1_785_578_400_000;
/// One join-risk window (60 s) in millis.
const WINDOW_MS: i64 = 60_000;
static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

fn schema_name_at(nanos: u128) -> String {
    format!(
        "join_risk_test_{}_{}_{}",
        std::process::id(),
        NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    schema: String,
}

impl TestDb {
    async fn new(setup: &[&'static str]) -> Result<Self, Box<dyn std::error::Error>> {
        // Fixed approved services, empty test password. Auth/ownership failures
        // propagate; no alternate credentials or fallback endpoints are tried.
        let host = if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
            "127.0.0.1"
        } else {
            "agent-testdb"
        };
        let options = PgConnectOptions::new()
            .host(host)
            .port(5432)
            .username("agent_test")
            .password("")
            .database("postgres")
            .ssl_mode(PgSslMode::Disable);
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.clone())
            .await?;
        let schema = schema_name_at(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos());
        QueryBuilder::<sqlx::Postgres>::new("CREATE SCHEMA ")
            .push(&schema)
            .build()
            .execute(&admin)
            .await?;
        let options = options.application_name(&schema);
        let pool = Self::connect(&options, &schema).await?;
        for sql in setup {
            sqlx::raw_sql(*sql).execute(&pool).await?;
        }
        Ok(Self {
            admin,
            pool,
            schema,
        })
    }

    async fn connect(options: &PgConnectOptions, schema: &str) -> Result<PgPool, sqlx::Error> {
        let path = schema.to_owned();
        PgPoolOptions::new()
            .max_connections(12)
            .acquire_timeout(Duration::from_secs(10))
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
            .connect_with(options.clone())
            .await
    }

    fn store(&self) -> JoinRiskStore {
        JoinRiskStore::from_pool(self.pool.clone())
    }

    /// Advisory locks are cluster-wide; per-schema guild IDs keep suites apart.
    fn guild(&self, suffix: &str) -> String {
        format!("{}-{suffix}", self.schema)
    }

    async fn finish(self) -> TestResult {
        self.pool.close().await;
        // Only the generated, run-owned schema is removed.
        QueryBuilder::<sqlx::Postgres>::new("DROP SCHEMA ")
            .push(&self.schema)
            .push(" CASCADE")
            .build()
            .execute(&self.admin)
            .await?;
        self.admin.close().await;
        Ok(())
    }
}

fn policy(guild: &str, bulk_until: Option<i64>) -> JoinRiskPolicy {
    JoinRiskPolicy::new(guild.to_owned(), 60.0, 5.0, bulk_until).expect("valid test tuning")
}

/// A fresh-account join (`age_ms` old) at `joined_ms` via `member`.
fn observation(
    policy: &JoinRiskPolicy,
    guild: &str,
    member: &str,
    joined_ms: i64,
    age_ms: i64,
) -> JoinRiskObservation {
    policy
        .prepare(
            &JoinRiskInput {
                guild_id: guild.to_owned(),
                member_id: member.to_owned(),
                member_is_bot: false,
                account_created_at_ms: joined_ms - age_ms,
                joined_at_ms: Some(joined_ms),
                source: "unknown".to_owned(),
            },
            joined_ms,
        )
        .expect("non-bot same-guild join always prepares")
}

/// Record and return the evidence; a duplicate is an error.
async fn persisted(
    store: &JoinRiskStore,
    observation: JoinRiskObservation,
    now_ms: i64,
) -> Result<(u64, two_bot_core::JoinRiskEvidence), Box<dyn std::error::Error>> {
    match store.record(observation, now_ms).await? {
        JoinRiskClaim::Persisted {
            evidence,
            join_count,
        } => Ok((join_count, evidence)),
        JoinRiskClaim::Duplicate => Err("unexpected duplicate".into()),
    }
}

struct StoredRow {
    score: i32,
    reasons_json: String,
    bulk_join_window: bool,
    flagged: bool,
    created_at: String,
}

async fn row(pool: &PgPool, id: &str) -> Result<StoredRow, sqlx::Error> {
    let (score, reasons_json, bulk_join_window, flagged, created_at) = sqlx::query_as(
        "SELECT score, reasons_json, bulk_join_window, flagged, created_at \
         FROM join_risk_flags WHERE event_id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok(StoredRow {
        score,
        reasons_json,
        bulk_join_window,
        flagged,
        created_at,
    })
}

#[tokio::test]
#[ignore = "requires the agent-testdb or CI Postgres service"]
async fn event_id_is_claimed_exactly_once_under_concurrency() -> TestResult {
    let db = TestDb::new(&[MIGRATION]).await?;
    let guild = db.guild("once");
    let policy = Arc::new(policy(&guild, None));
    let observation = observation(&policy, &guild, "member", NOW, 3_600_000);
    let barrier = Arc::new(Barrier::new(10));
    let mut tasks = Vec::new();
    for _ in 0..10 {
        let (store, observation, barrier) = (db.store(), observation.clone(), barrier.clone());
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            store.record(observation, NOW).await
        }));
    }
    let mut winners = 0;
    for task in tasks {
        match task.await?? {
            JoinRiskClaim::Persisted { .. } => winners += 1,
            JoinRiskClaim::Duplicate => {}
        }
    }
    assert_eq!(winners, 1, "one event ID must persist exactly once");
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM join_risk_flags WHERE event_id = $1")
        .bind(observation.event_id.as_str())
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(rows, 1);
    db.finish().await
}

#[tokio::test]
#[ignore = "requires the agent-testdb or CI Postgres service"]
async fn guild_join_counts_are_serialized_with_burst_scoring() -> TestResult {
    let db = TestDb::new(&[MIGRATION]).await?;
    let guild = db.guild("burst");
    let policy = Arc::new(policy(&guild, None));
    let barrier = Arc::new(Barrier::new(10));
    let mut tasks = Vec::new();
    for n in 0..10 {
        let observation = observation(&policy, &guild, &format!("member-{n:02}"), NOW, 3_600_000);
        let (store, barrier) = (db.store(), barrier.clone());
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            store.record(observation, NOW).await
        }));
    }
    let mut counts = BTreeSet::new();
    for task in tasks {
        let JoinRiskClaim::Persisted {
            evidence,
            join_count,
        } = task.await??
        else {
            return Err("distinct event IDs must all persist".into());
        };
        counts.insert(join_count);
        // Fresh accounts score 3 alone; the fifth concurrent join adds the
        // +2 burst bonus at threshold equality.
        assert_eq!(
            evidence.score,
            if join_count >= 5 { 5 } else { 3 },
            "join_count {join_count} scores exactly"
        );
        assert!(evidence.flagged);
        assert!(evidence.staff_message(true).is_some());
        assert!(evidence.staff_message(false).is_none());
    }
    // Each claim sees every earlier same-guild claim exactly once.
    assert_eq!(counts, (1..=10).collect::<BTreeSet<u64>>());

    // An old account needs the burst to flag: score 0 + 2 at the 11th join.
    let (count, evidence) = persisted(
        &db.store(),
        observation(&policy, &guild, "member-old", NOW, 30 * 86_400_000),
        NOW,
    )
    .await?;
    assert_eq!(count, 11);
    assert_eq!(evidence.score, 2);
    assert!(!evidence.flagged);
    assert!(evidence.staff_message(true).is_none());

    // A different guild in the same schema is isolated (legacy parity).
    let other = db.guild("isolation");
    let other_policy = policy(&other, None);
    let (count, _) = persisted(
        &db.store(),
        observation(&other_policy, &other, "member-00", NOW, 3_600_000),
        NOW,
    )
    .await?;
    assert_eq!(count, 1, "other-guild rows never count");
    db.finish().await
}

#[tokio::test]
#[ignore = "requires the agent-testdb or CI Postgres service"]
async fn stale_window_rows_and_bulk_suppression_behave() -> TestResult {
    let db = TestDb::new(&[MIGRATION]).await?;
    let guild = db.guild("window");
    let policy = policy(&guild, None);
    // A row whose processing-time `created_at` fell out of the window must not
    // count, even though its occurrence time is fresh (legacy WHERE clause).
    sqlx::query(
        "INSERT INTO join_risk_flags \
           (event_id, guild_id, member_id, account_created_at, joined_at, source, score, \
            reasons_json, bulk_join_window, flagged, created_at) \
         VALUES ($1, $2, 'stale-member', $3, $4, 'unknown', 3, '[\"x\"]', false, true, $5)",
    )
    .bind("stale-row")
    .bind(&guild)
    .bind(two_bot_core::format_iso_millis(NOW - 3_600_000))
    .bind(two_bot_core::format_iso_millis(NOW))
    .bind(two_bot_core::format_iso_millis(NOW - WINDOW_MS - 1))
    .execute(&db.pool)
    .await?;
    let (count, _) = persisted(
        &db.store(),
        observation(&policy, &guild, "member-00", NOW, 3_600_000),
        NOW,
    )
    .await?;
    assert_eq!(count, 1, "out-of-window created_at rows never count");

    // Bulk suppression preserves the score but never flags; the row persists
    // and still counts toward later joins (legacy parity).
    let bulk_policy = policy(&guild, Some(NOW));
    let observation = observation(&bulk_policy, &guild, "member-01", NOW, 3_600_000);
    assert!(observation.bulk_join_window);
    let (count, evidence) = persisted(&db.store(), observation.clone(), NOW).await?;
    assert_eq!(count, 2);
    assert_eq!(evidence.score, 3);
    assert!(!evidence.flagged);
    assert!(evidence.staff_message(true).is_none());
    let stored = row(&db.pool, &observation.event_id).await?;
    assert_eq!(stored.score, 3);
    assert_eq!(
        stored.reasons_json,
        serde_json::to_string(&evidence.reasons).expect("test reasons serialize")
    );
    assert!(stored.bulk_join_window, "bulk_join_window persists exactly");
    assert!(!stored.flagged, "flagged persists exactly");
    assert_eq!(
        stored.created_at,
        two_bot_core::format_iso_millis(NOW),
        "created_at is the processing instant"
    );

    // The duplicate replays nothing: no recount, no second row.
    assert_eq!(
        db.store().record(observation, NOW).await?,
        JoinRiskClaim::Duplicate
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM join_risk_flags WHERE guild_id = $1")
        .bind(&guild)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(rows, 3);
    db.finish().await
}

async fn shape(pool: &PgPool, schema: &str) -> Result<Vec<String>, sqlx::Error> {
    let mut shape: Vec<String> = sqlx::query_scalar(
        "SELECT table_name || '.' || column_name || ':' || data_type || ':' || is_nullable || ':' || ordinal_position \
         FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'join_risk_flags' \
         ORDER BY table_name, ordinal_position",
    )
    .fetch_all(pool)
    .await?;
    shape.extend(
        sqlx::query_scalar::<_, String>(
            "SELECT c.conname || ':' || pg_get_constraintdef(c.oid) FROM pg_constraint c \
             JOIN pg_class t ON t.oid = c.conrelid JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = current_schema() AND t.relname = 'join_risk_flags' ORDER BY c.conname",
        )
        .fetch_all(pool)
        .await?,
    );
    shape.extend(
        sqlx::query_scalar::<_, String>(
            "SELECT indexdef FROM pg_indexes \
             WHERE schemaname = current_schema() AND tablename = 'join_risk_flags' ORDER BY indexname",
        )
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|definition| definition.replace(schema, "<schema>")),
    );
    Ok(shape)
}

#[tokio::test]
#[ignore = "requires the agent-testdb or CI Postgres service"]
async fn upgrading_from_legacy_0015_loses_nothing() -> TestResult {
    let legacy = TestDb::new(&[LEGACY_0015]).await?;
    // Legacy event IDs already use the portable `guild:member:joinedAt` shape,
    // so a replay of the same join collides on the primary key.
    sqlx::query(
        "INSERT INTO join_risk_flags \
           (event_id, guild_id, member_id, account_created_at, joined_at, source, score, \
            reasons_json, bulk_join_window, flagged, created_at) \
         VALUES ('legacy-guild:legacy-member:2026-08-01T09:59:00.000Z', 'legacy-guild', \
           'legacy-member', \
           '2020-01-01T00:00:00.000Z', '2026-08-01T09:59:00.000Z', 'unknown', \
           5, '[\"account younger than 24 hours\", \"5 joins inside 60s\"]', false, true, \
           '2026-08-01T09:59:00.100Z')",
    )
    .execute(&legacy.pool)
    .await?;
    let before: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(j)::text FROM (SELECT * FROM join_risk_flags ORDER BY event_id) j",
    )
    .fetch_all(&legacy.pool)
    .await?;
    assert_eq!(before.len(), 1);
    let legacy_shape = shape(&legacy.pool, &legacy.schema).await?;
    sqlx::raw_sql(MIGRATION).execute(&legacy.pool).await?;
    let after: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(j)::text FROM (SELECT * FROM join_risk_flags ORDER BY event_id) j",
    )
    .fetch_all(&legacy.pool)
    .await?;
    assert_eq!(after, before, "0360 must not alter legacy rows");
    assert_eq!(shape(&legacy.pool, &legacy.schema).await?, legacy_shape);

    let fresh = TestDb::new(&[MIGRATION]).await?;
    assert_eq!(
        shape(&fresh.pool, &fresh.schema).await?,
        legacy_shape,
        "a fresh 0360 schema matches the legacy shape"
    );
    fresh.finish().await?;

    // The store reads legacy rows: the legacy event ID stays claimed.
    let store = legacy.store();
    let guild = "legacy-guild";
    let policy = policy(guild, None);
    let replay = policy
        .prepare(
            &JoinRiskInput {
                guild_id: guild.to_owned(),
                member_id: "legacy-member".to_owned(),
                member_is_bot: false,
                account_created_at_ms: 1_577_836_800_000,
                joined_at_ms: Some(1_785_578_340_000),
                source: "unknown".to_owned(),
            },
            1_785_578_340_000,
        )
        .expect("replay prepares");
    assert_eq!(
        replay.event_id,
        "legacy-guild:legacy-member:2026-08-01T09:59:00.000Z"
    );
    assert_eq!(
        store.record(replay, 1_785_578_340_000).await?,
        JoinRiskClaim::Duplicate,
        "legacy event IDs stay claimed"
    );
    legacy.finish().await
}
