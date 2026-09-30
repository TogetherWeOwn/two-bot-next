//! Explicit DB regressions: agent-testdb locally, the Postgres CI service in
//! GitHub Actions. No app DB URL, credentials, migrations in public, or Discord.
//! Run: cargo test -p two-bot-cutover --test settings_db --locked -- --ignored

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{Pool, Postgres, QueryBuilder};
use two_bot_core::internal_actions::ErrorCode;
use two_bot_core::internal_settings::SettingsCommand;
use two_bot_core::settings::SettingsCache;
use two_bot_cutover::internal_settings::execute_settings;
use two_bot_cutover::settings::SettingsStore;

const KEY: &str = "TWO_RAID_JOIN_THRESHOLD";
const OTHER_KEY: &str = "TWO_RAID_WINDOW_SECONDS";
type TestResult = Result<(), Box<dyn std::error::Error>>;
type AuditRow = (Option<Value>, Option<Value>);

struct TestDb {
    admin: Pool<Postgres>,
    pool: Pool<Postgres>,
    schema: String,
}

impl TestDb {
    async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let db = Self::new_legacy().await?;
        db.migrate_row_versions().await?;
        Ok(db)
    }

    async fn migrate_row_versions(&self) -> Result<(), sqlx::Error> {
        for migration in [
            include_str!("../migrations/0331_guild_settings_versions.sql"),
            include_str!("../migrations/0332_guild_settings_allocator.sql"),
            include_str!("../migrations/0333_guild_settings_revision.sql"),
        ] {
            self.migrate(migration).await?;
        }
        Ok(())
    }

    async fn migrate(&self, migration: &'static str) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::raw_sql(migration).execute(&mut *tx).await?;
        tx.commit().await
    }

    async fn new_legacy() -> Result<Self, Box<dyn std::error::Error>> {
        // The endpoint is chosen here, never from DATABASE_URL or any app
        // config. CI uses only its disposable trust-authenticated service.
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
        let schema = format!(
            "settings_test_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        // Identifier is a constant prefix plus numeric process/time IDs only.
        QueryBuilder::<Postgres>::new("CREATE SCHEMA ")
            .push(&schema)
            .build()
            .execute(&admin)
            .await?;
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
            .connect_with(options.application_name(&schema))
            .await?;
        sqlx::raw_sql(include_str!("../migrations/0330_guild_settings.sql"))
            .execute(&pool)
            .await?;
        Ok(Self {
            admin,
            pool,
            schema,
        })
    }

    async fn audit(&self) -> Result<Vec<AuditRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT old_value, new_value FROM guild_settings_audit WHERE key = $1 ORDER BY id",
        )
        .bind(KEY)
        .fetch_all(&self.pool)
        .await
    }

    async fn wait_for_writers(&self, count: i64) -> TestResult {
        // Rendezvous on actual database lock waits, not an assumed sleep long
        // enough for writers to reach their old-value reads.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let waiting: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM pg_stat_activity
                     WHERE application_name = $1 AND cardinality(pg_blocking_pids(pid)) > 0",
                )
                .bind(&self.schema)
                .fetch_one(&self.admin)
                .await?;
                if waiting == count {
                    return Ok::<_, sqlx::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        Ok(())
    }

    async fn finish(self) -> TestResult {
        self.pool.close().await;
        // Only this test's generated schema; never truncate shared audit data.
        QueryBuilder::<Postgres>::new("DROP SCHEMA ")
            .push(&self.schema)
            .push(" CASCADE")
            .build()
            .execute(&self.admin)
            .await?;
        self.admin.close().await;
        Ok(())
    }
}

const GUILD: &str = "111111111111111111";
const ADMIN: &str = "222222222222222222";

fn command(action: &str, body: Value) -> SettingsCommand {
    SettingsCommand::parse(action, body.as_object().unwrap()).unwrap()
}

fn save(value: Value, expected_version: Option<i64>) -> SettingsCommand {
    let mut body = json!({"key": KEY, "value": value, "updated_by": ADMIN});
    if let Some(version) = expected_version {
        body["expected_version"] = json!(version);
    }
    command("settings.set", body)
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn internal_settings_get_set_delete_preserve_wire_actor_audit_and_poll_version() -> TestResult
{
    let db = TestDb::new().await?;
    let store = SettingsStore::new(&db.pool);
    let read = command("settings.get", json!({"key": KEY}));
    // A configured fallback is intentionally not returned by settings.get.
    std::env::set_var(KEY, "environment-fallback-must-not-be-returned");
    let initial = execute_settings(&store, GUILD, &read).await;
    std::env::remove_var(KEY);
    let initial = initial?;
    assert_eq!(
        initial.result,
        json!({"key": KEY, "value": null, "source": "unset"})
    );
    assert_eq!(initial.observed_version, 0);
    let before = store.poll_marks().await?;

    let saved = execute_settings(&store, GUILD, &save(json!("8"), Some(0))).await?;
    assert_eq!(saved.result, json!({"key": KEY, "outcome": "saved"}));
    assert!(saved.observed_version > 0);
    assert!(store.poll_marks().await?.0 > before.0);
    let stored = execute_settings(&store, GUILD, &read).await?;
    assert_eq!(
        stored.result.to_string(),
        r#"{"key":"TWO_RAID_JOIN_THRESHOLD","value":"8","source":"store"}"#
    );
    assert_eq!(stored.observed_version, saved.observed_version);
    let actor: String =
        sqlx::query_scalar("SELECT updated_by FROM guild_settings WHERE guild_id = $1")
            .bind(GUILD)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(actor, ADMIN);
    let other = execute_settings(&store, "333333333333333333", &read).await?;
    assert_eq!(other.result["source"], "unset");

    let updated =
        execute_settings(&store, GUILD, &save(json!(9), Some(saved.observed_version))).await?;
    assert!(updated.observed_version > saved.observed_version);
    let revision = store.poll_marks().await?.0;
    let deleted = execute_settings(
        &store,
        GUILD,
        &save(Value::Null, Some(updated.observed_version)),
    )
    .await?;
    assert_eq!(
        deleted.result.to_string(),
        r#"{"key":"TWO_RAID_JOIN_THRESHOLD","outcome":"unset"}"#
    );
    assert_eq!(deleted.observed_version, 0);
    assert!(store.poll_marks().await?.0 > revision);
    assert_eq!(
        execute_settings(&store, GUILD, &read).await?.result["source"],
        "unset"
    );
    let audit: Vec<(Option<Value>, Option<Value>, String)> = sqlx::query_as(
        "SELECT old_value, new_value, actor FROM guild_settings_audit WHERE guild_id = $1 ORDER BY id",
    ).bind(GUILD).fetch_all(&db.pool).await?;
    assert_eq!(
        audit,
        vec![
            (None, Some(json!("8")), ADMIN.into()),
            (Some(json!("8")), Some(json!(9)), ADMIN.into()),
            (Some(json!(9)), None, ADMIN.into()),
        ]
    );
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn internal_settings_stale_save_and_delete_leave_no_side_effects() -> TestResult {
    let db = TestDb::new().await?;
    let store = SettingsStore::new(&db.pool);
    let saved = execute_settings(&store, GUILD, &save(json!(8), Some(0))).await?;
    let newer =
        execute_settings(&store, GUILD, &save(json!(9), Some(saved.observed_version))).await?;
    let marks = store.poll_marks().await?;
    for value in [json!(10), Value::Null] {
        let error = execute_settings(&store, GUILD, &save(value, Some(saved.observed_version)))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::VersionConflict);
        assert_eq!(error.status(), 409);
        assert!(!error.code.retryable());
        assert_eq!(store.poll_marks().await?, marks);
        assert_eq!(
            store.get(GUILD, KEY).await?,
            Some((json!(9), newer.observed_version))
        );
        assert_eq!(db.audit().await?.len(), 2);
    }
    // A stale non-zero version cannot recreate a deleted row.
    execute_settings(
        &store,
        GUILD,
        &save(Value::Null, Some(newer.observed_version)),
    )
    .await?;
    let marks = store.poll_marks().await?;
    assert_eq!(
        execute_settings(
            &store,
            GUILD,
            &save(json!(11), Some(newer.observed_version))
        )
        .await
        .unwrap_err()
        .code,
        ErrorCode::VersionConflict
    );
    assert_eq!(store.poll_marks().await?, marks);
    assert_eq!(db.audit().await?.len(), 3);
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn direct_sql_updates_invalidate_stale_save_and_delete_tokens() -> TestResult {
    let db = TestDb::new().await?;
    let store = SettingsStore::new(&db.pool);
    let mut token = execute_settings(&store, GUILD, &save(json!(8), Some(0)))
        .await?
        .observed_version;
    for statement in [
        "UPDATE guild_settings SET value = '9' WHERE guild_id = $1 AND key = $2",
        "INSERT INTO guild_settings (guild_id, key, value, version, updated_by)
         VALUES ($1, $2, '9', 1, 'direct-writer')
         ON CONFLICT (guild_id, key) DO UPDATE SET value = EXCLUDED.value, version = EXCLUDED.version",
        "UPDATE guild_settings SET version = 1 WHERE guild_id = $1 AND key = $2",
    ] {
        sqlx::query(statement)
            .bind(GUILD)
            .bind(KEY)
            .execute(&db.pool)
            .await?;
        let current = store.get(GUILD, KEY).await?.unwrap();
        assert_eq!(current.0, json!(9));
        assert!(current.1 > token, "even supplied versions must be replaced");
        let marks = store.poll_marks().await?;
        for value in [json!(10), Value::Null] {
            let error = execute_settings(&store, GUILD, &save(value, Some(token)))
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::VersionConflict);
            assert_eq!(store.get(GUILD, KEY).await?, Some(current.clone()));
            assert_eq!(store.poll_marks().await?, marks);
            assert_eq!(db.audit().await?.len(), 1);
        }
        token = current.1;
    }
    // The fresh token is usable, and the audit records the direct writer's value.
    let saved = execute_settings(&store, GUILD, &save(json!(10), Some(token))).await?;
    assert!(saved.observed_version > token);
    assert_eq!(db.audit().await?[1], (Some(json!(9)), Some(json!(10))));
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn version_migration_preserves_rows_and_seeds_above_existing_tokens() -> TestResult {
    let db = TestDb::new_legacy().await?;
    // Both an extant manually assigned token and an allocated but unused token
    // must remain below every future database-owned version.
    sqlx::query(
        "INSERT INTO guild_settings (guild_id, key, value, version, updated_by)
         VALUES ($1, $2, '8', 1000000, 'legacy-writer')",
    )
    .bind(GUILD)
    .bind(KEY)
    .execute(&db.pool)
    .await?;
    let store = SettingsStore::new(&db.pool);
    let before = store.poll_marks().await?;
    db.migrate_row_versions().await?;
    assert_eq!(store.get(GUILD, KEY).await?, Some((json!(8), 1000000)));
    assert_eq!(store.poll_marks().await?, before);
    sqlx::query("UPDATE guild_settings SET value = '9' WHERE guild_id = $1 AND key = $2")
        .bind(GUILD)
        .bind(KEY)
        .execute(&db.pool)
        .await?;
    let token = store.get(GUILD, KEY).await?.unwrap().1;
    assert!(token > 1000000);
    assert_eq!(
        execute_settings(&store, GUILD, &save(json!(10), Some(1000000)))
            .await
            .unwrap_err()
            .code,
        ErrorCode::VersionConflict
    );
    assert!(db.audit().await?.is_empty());
    db.finish().await?;

    let db = TestDb::new_legacy().await?;
    sqlx::query("SELECT setval('guild_settings_version_seq', 2000000, true)")
        .execute(&db.pool)
        .await?;
    db.migrate_row_versions().await?;
    let saved = execute_settings(
        &SettingsStore::new(&db.pool),
        GUILD,
        &save(json!(8), Some(0)),
    )
    .await?;
    assert!(saved.observed_version > 2000000, "allocator never rewinds");
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn schema_binding_upgrade_reseeds_shadow_tokens_and_blocks_dml() -> TestResult {
    for unused_allocations in [false, true] {
        let db = TestDb::new_legacy().await?;
        db.migrate(include_str!(
            "../migrations/0331_guild_settings_versions.sql"
        ))
        .await?;
        sqlx::query("SELECT nextval('guild_settings_version_seq') FROM generate_series(1, 9)")
            .execute(&db.pool)
            .await?;
        let canonical: i64 =
            sqlx::query_scalar("SELECT last_value FROM guild_settings_version_seq")
                .fetch_one(&db.pool)
                .await?;
        let mut tx = db.pool.begin().await?;
        sqlx::query("CREATE TEMP SEQUENCE guild_settings_version_seq")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "SELECT nextval('pg_temp.guild_settings_version_seq') FROM generate_series(1::bigint, $1)",
        )
        .bind(canonical)
        .execute(&mut *tx)
        .await?;
        sqlx::query("SELECT set_config('search_path', $1, true)")
            .bind(format!("pg_temp,{}", db.schema))
            .execute(&mut *tx)
            .await?;
        // A real intervening write on installed 0331, before 0332 is applied.
        let shadow_token: i64 = QueryBuilder::<Postgres>::new("INSERT INTO ")
            .push(&db.schema)
            .push(".guild_settings (guild_id, key, value, updated_by) VALUES (")
            .push_bind(GUILD)
            .push(", ")
            .push_bind(KEY)
            .push(", '8', 'legacy-writer') RETURNING version")
            .build_query_scalar()
            .fetch_one(&mut *tx)
            .await?;
        assert_eq!(shadow_token, canonical + 1);
        sqlx::query("DROP SEQUENCE pg_temp.guild_settings_version_seq")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        db.migrate(include_str!(
            "../migrations/0332_guild_settings_allocator.sql"
        ))
        .await?;
        if unused_allocations {
            sqlx::query("SELECT nextval('guild_settings_version_seq') FROM generate_series(1, 25)")
                .execute(&db.pool)
                .await?;
        }
        let allocated: i64 =
            sqlx::query_scalar("SELECT last_value FROM guild_settings_version_seq")
                .fetch_one(&db.pool)
                .await?;
        let store = SettingsStore::new(&db.pool);
        let before = store.poll_marks().await?;
        let mut upgrade = db.pool.begin().await?;
        sqlx::raw_sql(include_str!(
            "../migrations/0333_guild_settings_revision.sql"
        ))
        .execute(&mut *upgrade)
        .await?;
        assert_eq!(store.get(GUILD, KEY).await?, Some((json!(8), shadow_token)));
        assert_eq!(store.poll_marks().await?, before);
        assert!(db.audit().await?.is_empty());

        // The store acquires the revision lock before waiting on the DML lock.
        // Upgrade commit must not need that revision lock or deadlock with it.
        let pool = db.pool.clone();
        let writer = tokio::spawn(async move {
            execute_settings(
                &SettingsStore::new(&pool),
                GUILD,
                &save(json!(9), Some(shadow_token)),
            )
            .await
        });
        db.wait_for_writers(1).await?;
        upgrade.commit().await?;
        let saved = tokio::time::timeout(Duration::from_secs(5), writer).await???;
        assert!(saved.observed_version > shadow_token.max(allocated));
        let marks = store.poll_marks().await?;
        for value in [json!(10), Value::Null] {
            let error = execute_settings(&store, GUILD, &save(value, Some(shadow_token)))
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::VersionConflict);
            assert_eq!(error.status(), 409);
            assert!(!error.code.retryable());
            assert_eq!(
                store.get(GUILD, KEY).await?,
                Some((json!(9), saved.observed_version))
            );
            assert_eq!(store.poll_marks().await?, marks);
            assert_eq!(db.audit().await?, vec![(Some(json!(8)), Some(json!(9)))]);
        }
        execute_settings(
            &store,
            GUILD,
            &save(json!(10), Some(saved.observed_version)),
        )
        .await?;
        assert_eq!(db.audit().await?[1], (Some(json!(9)), Some(json!(10))));
        db.finish().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn legacy_zero_version_does_not_match_absence_after_upgrade() -> TestResult {
    let db = TestDb::new_legacy().await?;
    sqlx::query(
        "INSERT INTO guild_settings (guild_id, key, value, version, updated_by)
         VALUES ($1, $2, '8', 0, 'legacy-writer')",
    )
    .bind(GUILD)
    .bind(KEY)
    .execute(&db.pool)
    .await?;
    db.migrate_row_versions().await?;
    let store = SettingsStore::new(&db.pool);
    let read = command("settings.get", json!({"key": KEY}));
    let observed = execute_settings(&store, GUILD, &read).await?;
    assert_eq!(
        observed.result,
        json!({"key": KEY, "value": 8, "source": "store"})
    );
    assert_eq!(
        observed.observed_version, 0,
        "upgrade does not rewrite row tokens"
    );
    let marks = store.poll_marks().await?;
    for value in [json!(9), Value::Null] {
        let error = execute_settings(&store, GUILD, &save(value, Some(0)))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::VersionConflict);
        assert_eq!(error.status(), 409);
        assert!(!error.code.retryable());
        assert_eq!(store.get(GUILD, KEY).await?, Some((json!(8), 0)));
        assert_eq!(store.poll_marks().await?, marks);
        assert!(db.audit().await?.is_empty());
    }
    // Only a deliberate legacy unconditional save can update this old row.
    let saved = execute_settings(&store, GUILD, &save(json!(9), None)).await?;
    assert!(saved.observed_version > 0);
    execute_settings(
        &store,
        GUILD,
        &save(Value::Null, Some(saved.observed_version)),
    )
    .await?;
    assert_eq!(store.get(GUILD, KEY).await?, None);
    assert_eq!(
        db.audit().await?,
        vec![(Some(json!(8)), Some(json!(9))), (Some(json!(9)), None)]
    );
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn qualified_sql_uses_target_allocator_and_revision_not_shadows() -> TestResult {
    let db = TestDb::new().await?;
    let store = SettingsStore::new(&db.pool);
    let saved = execute_settings(&store, GUILD, &save(json!(8), Some(0))).await?;
    let before = store.poll_marks().await?;
    let mut cache = SettingsCache::load(&store.load_snapshot().await?);
    let mut tx = db.pool.begin().await?;
    sqlx::raw_sql(
        "CREATE TEMP TABLE guild_settings_revision (singleton BOOLEAN PRIMARY KEY, revision BIGINT);
         INSERT INTO guild_settings_revision VALUES (TRUE, 41);",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("CREATE TEMP SEQUENCE guild_settings_version_seq")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_catalog.setval('pg_temp.guild_settings_version_seq', $1, false)")
        .bind(saved.observed_version)
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_catalog.set_config('search_path', $1, true)")
        .bind(format!("pg_temp,{}", db.schema))
        .execute(&mut *tx)
        .await?;
    // Only a generated, numeric schema identifier is interpolated. The caller
    // qualifies the table while a same-named sequence precedes it in lookup.
    let token: i64 = QueryBuilder::<Postgres>::new("UPDATE ")
        .push(&db.schema)
        .push(".guild_settings SET value = '9' WHERE guild_id = ")
        .push_bind(GUILD)
        .push(" AND key = ")
        .push_bind(KEY)
        .push(" RETURNING version")
        .build_query_scalar()
        .fetch_one(&mut *tx)
        .await?;
    assert!(
        token > saved.observed_version,
        "shadow sequence cannot preserve a stale token"
    );
    let shadow: (i64, bool) =
        sqlx::query_as("SELECT last_value, is_called FROM pg_temp.guild_settings_version_seq")
            .fetch_one(&mut *tx)
            .await?;
    assert_eq!(
        shadow,
        (saved.observed_version, false),
        "shadow sequence was never used"
    );
    let shadow_revision: i64 =
        sqlx::query_scalar("SELECT revision FROM pg_temp.guild_settings_revision")
            .fetch_one(&mut *tx)
            .await?;
    assert_eq!(shadow_revision, 41, "shadow revision was never changed");
    assert_eq!(
        store.poll_marks().await?,
        before,
        "uncommitted is invisible"
    );
    sqlx::raw_sql(
        "DROP SEQUENCE pg_temp.guild_settings_version_seq;
         DROP TABLE pg_temp.guild_settings_revision;",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let marks = store.poll_marks().await?;
    assert_eq!(marks, (before.0 + 1, before.1));
    assert!(cache.needs_refresh(marks.0, marks.1));
    cache.refresh(&store.load_snapshot().await?);
    assert_eq!(cache.get(GUILD, KEY), Some(&json!(9)));
    for value in [json!(10), Value::Null] {
        let error = execute_settings(&store, GUILD, &save(value, Some(saved.observed_version)))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::VersionConflict);
        assert_eq!(store.get(GUILD, KEY).await?, Some((json!(9), token)));
        assert_eq!(store.poll_marks().await?, marks);
        assert_eq!(db.audit().await?.len(), 1);
    }
    // A normal caller can use the fresh token after the qualified direct write.
    execute_settings(&store, GUILD, &save(json!(10), Some(token))).await?;
    assert_eq!(db.audit().await?[1], (Some(json!(9)), Some(json!(10))));
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn qualified_writer_waits_on_target_revision_lock() -> TestResult {
    let db = TestDb::new().await?;
    let store = SettingsStore::new(&db.pool);
    let saved = execute_settings(&store, GUILD, &save(json!(8), Some(0))).await?;
    let before = store.poll_marks().await?;
    let mut gate = db.pool.begin().await?;
    sqlx::query("SELECT revision FROM guild_settings_revision WHERE singleton = TRUE FOR UPDATE")
        .fetch_one(&mut *gate)
        .await?;
    let pool = db.pool.clone();
    let schema = db.schema.clone();
    let writer = tokio::spawn(async move {
        let mut tx = pool.begin().await?;
        sqlx::raw_sql(
            "CREATE TEMP TABLE guild_settings_revision (singleton BOOLEAN PRIMARY KEY, revision BIGINT);
             INSERT INTO guild_settings_revision VALUES (TRUE, 41);",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("SELECT set_config('search_path', $1, true)")
            .bind(format!("pg_temp,{schema}"))
            .execute(&mut *tx)
            .await?;
        let token: i64 = QueryBuilder::<Postgres>::new("UPDATE ")
            .push(&schema)
            .push(".guild_settings SET value = '9' WHERE guild_id = ")
            .push_bind(GUILD)
            .push(" AND key = ")
            .push_bind(KEY)
            .push(" RETURNING version")
            .build_query_scalar()
            .fetch_one(&mut *tx)
            .await?;
        let shadow: i64 =
            sqlx::query_scalar("SELECT revision FROM pg_temp.guild_settings_revision")
                .fetch_one(&mut *tx)
                .await?;
        assert_eq!(shadow, 41);
        sqlx::query("DROP TABLE pg_temp.guild_settings_revision")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(token)
    });
    db.wait_for_writers(1).await?;
    assert_eq!(
        store.get(GUILD, KEY).await?,
        Some((json!(8), saved.observed_version))
    );
    assert_eq!(store.poll_marks().await?, before);
    gate.commit().await?;
    let token = tokio::time::timeout(Duration::from_secs(5), writer).await???;
    assert!(token > saved.observed_version);
    assert_eq!(store.poll_marks().await?, (before.0 + 1, before.1));
    assert_eq!(store.get(GUILD, KEY).await?, Some((json!(9), token)));
    execute_settings(&store, GUILD, &save(json!(10), Some(token))).await?;
    assert_eq!(db.audit().await?[1], (Some(json!(9)), Some(json!(10))));
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn missing_target_revision_refuses_qualified_write_despite_shadow() -> TestResult {
    let db = TestDb::new().await?;
    let store = SettingsStore::new(&db.pool);
    let saved = execute_settings(&store, GUILD, &save(json!(8), Some(0))).await?;
    let before = store.poll_marks().await?;
    let mut tx = db.pool.begin().await?;
    sqlx::raw_sql(
        "CREATE TEMP TABLE guild_settings_revision (singleton BOOLEAN PRIMARY KEY, revision BIGINT);
         INSERT INTO guild_settings_revision VALUES (TRUE, 41);",
    )
    .execute(&mut *tx)
    .await?;
    QueryBuilder::<Postgres>::new("DELETE FROM ")
        .push(&db.schema)
        .push(".guild_settings_revision")
        .build()
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT set_config('search_path', $1, true)")
        .bind(format!("pg_temp,{}", db.schema))
        .execute(&mut *tx)
        .await?;
    let error = QueryBuilder::<Postgres>::new("UPDATE ")
        .push(&db.schema)
        .push(".guild_settings SET value = '9'")
        .build()
        .execute(&mut *tx)
        .await
        .unwrap_err();
    let sqlx::Error::Database(error) = error else {
        panic!("expected missing revision rejection");
    };
    assert_eq!(error.code().as_deref(), Some("P0001"));
    assert_eq!(error.message(), "guild_settings revision row is missing");
    tx.rollback().await?;
    assert_eq!(
        store.get(GUILD, KEY).await?,
        Some((json!(8), saved.observed_version))
    );
    assert_eq!(store.poll_marks().await?, before);
    assert_eq!(db.audit().await?, vec![(None, Some(json!(8)))]);
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn json_nul_refusals_preserve_value_version_revision_and_audit() -> TestResult {
    let db = TestDb::new().await?;
    let store = SettingsStore::new(&db.pool);
    let saved = execute_settings(&store, GUILD, &save(json!(8), Some(0))).await?;
    let marks = store.poll_marks().await?;
    for value in [
        json!("private-value\0"),
        json!([{"nested": ["private-value\0"]}]),
        json!({"nested": {"private-key\0": 8}}),
    ] {
        let body = json!({"key": KEY, "value": value, "updated_by": ADMIN});
        let error = SettingsCommand::parse("settings.set", body.as_object().unwrap()).unwrap_err();
        assert_eq!(error.code, ErrorCode::Malformed);
        assert_eq!(error.status(), 400);
        assert!(!error.code.retryable());
        assert!(!format!("{error:?}").contains("private-"));
        // Alternate SettingsStore writers have the same pre-SQL guard.
        let error = store.set(GUILD, KEY, Some(value), ADMIN).await.unwrap_err();
        assert!(matches!(
            error,
            two_bot_cutover::settings::SettingsWriteError::Refused(
                two_bot_core::settings::WriteRefusal::NullCharacter
            )
        ));
        assert_eq!(
            store.get(GUILD, KEY).await?,
            Some((json!(8), saved.observed_version))
        );
        assert_eq!(store.poll_marks().await?, marks);
        assert_eq!(db.audit().await?.len(), 1);
    }
    let valid = json!({"nested": ["literal \\u0000", "unicode \u{1}é😀", null]});
    execute_settings(
        &store,
        GUILD,
        &save(valid.clone(), Some(saved.observed_version)),
    )
    .await?;
    assert_eq!(store.get(GUILD, KEY).await?.unwrap().0, valid);
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn internal_settings_concurrent_create_has_one_winner_and_one_audit() -> TestResult {
    let db = TestDb::new().await?;
    let store = SettingsStore::new(&db.pool);
    let first = save(json!(8), Some(0));
    let second = save(json!(9), Some(0));
    let (a, b) = tokio::join!(
        execute_settings(&store, GUILD, &first),
        execute_settings(&store, GUILD, &second),
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let loser = match (a, b) {
        (Err(error), Ok(_)) | (Ok(_), Err(error)) => error,
        other => panic!("expected one winner and one conflict: {other:?}"),
    };
    assert_eq!(loser.code, ErrorCode::VersionConflict);
    assert_eq!(db.audit().await?.len(), 1);
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn internal_settings_refusals_and_database_failure_do_not_leak_or_mutate() -> TestResult {
    let db = TestDb::new().await?;
    let store = SettingsStore::new(&db.pool);
    let marks = store.poll_marks().await?;
    for key in [
        "DISCORD_TOKEN",
        "TWO_INTERNAL_KEYS",
        "TWO_MODERATION",
        "UNKNOWN_KEY",
    ] {
        for action in ["settings.get", "settings.set"] {
            let body = json!({"key": key, "value": "private-value", "updated_by": ADMIN});
            let error = SettingsCommand::parse(action, body.as_object().unwrap()).unwrap_err();
            assert_eq!(error.code, ErrorCode::ActionNotAllowed);
            assert!(!format!("{error:?}").contains("private-value"));
        }
        assert!(store.get(GUILD, key).await.is_err());
        assert!(store
            .set(GUILD, key, Some(json!("private-value")), ADMIN)
            .await
            .is_err());
    }
    assert_eq!(store.poll_marks().await?, marks);
    assert!(db.audit().await?.is_empty());

    // Force a DB error whose detail contains the submitted row. The action
    // boundary must sanitize it, and the audit failure must roll back the save.
    sqlx::query("ALTER TABLE guild_settings_audit ADD CONSTRAINT reject_test_actor CHECK (actor <> '222222222222222222')")
        .execute(&db.pool).await?;
    let error = execute_settings(&store, GUILD, &save(json!("private-value"), None))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(!format!("{error:?}").contains("private-value"));
    assert_eq!(store.get(GUILD, KEY).await?, None);
    assert_eq!(store.poll_marks().await?, marks);
    assert!(db.audit().await?.is_empty());
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn numeric_settings_round_trip_into_integer_config_readers() -> TestResult {
    let db = TestDb::new().await?;
    let store = SettingsStore::new(&db.pool);
    let key = "TWO_AUTOMOD_REPEAT_COUNT";
    let mut cache = SettingsCache::default();
    for literal in ["3", "3.0", "3e0"] {
        store
            .set("g1", key, Some(serde_json::from_str(literal)?), "writer")
            .await?;
        let marks = store.poll_marks().await?;
        assert!(cache.needs_refresh(marks.0, marks.1));
        cache.refresh(&store.load_snapshot().await?);
        let env = cache.env_snapshot(Some("g1"));
        assert_eq!(env.get(key).map(String::as_str), Some("3"), "{literal}");
        let config = two_bot_core::AutomodConfig::from_map(&env)?;
        assert_eq!(config.policy.repeated_message_count, 3, "{literal}");
    }
    store.set("g1", key, Some(json!(3.5)), "writer").await?;
    cache.refresh(&store.load_snapshot().await?);
    assert!(two_bot_core::AutomodConfig::from_map(&cache.env_snapshot(Some("g1"))).is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM guild_settings_audit WHERE key = $1")
        .bind(key)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(count, 4, "one audit per numeric write");

    let id_key = "DISCORD_AUDIT_LOG_CHANNEL_ID";
    for value in [9_007_199_254_740_993_u64, u64::MAX] {
        store
            .set("g1", id_key, Some(json!(value)), "writer")
            .await?;
        cache.refresh(&store.load_snapshot().await?);
        assert_eq!(
            cache.env_snapshot(Some("g1")).get(id_key),
            Some(&value.to_string()),
            "integer IDs must survive the DB/cache/renderer without f64 rounding"
        );
    }
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn supplied_versions_are_replaced_and_same_count_delete_is_seen_by_one_poll() -> TestResult {
    let db = TestDb::new().await?;
    let store = SettingsStore::new(&db.pool);
    store.set("g1", KEY, Some(json!(1)), "seed").await?;
    let mut slow = db.pool.begin().await?;
    let lower: i64 = sqlx::query_scalar("SELECT nextval('guild_settings_version_seq')")
        .fetch_one(&mut *slow)
        .await?;
    store.set("g1", OTHER_KEY, Some(json!(9)), "fast").await?;
    let before = store.poll_marks().await?;
    let max_before: i64 = sqlx::query_scalar("SELECT max(version) FROM guild_settings")
        .fetch_one(&db.pool)
        .await?;
    assert!(lower < max_before);
    let mut cache = SettingsCache::load(&store.load_snapshot().await?);

    // An old caller-supplied sequence must not revive a prior row token.
    sqlx::query("UPDATE guild_settings SET value = '2', version = $1 WHERE key = $2")
        .bind(lower)
        .bind(KEY)
        .execute(&mut *slow)
        .await?;
    assert_eq!(
        store.poll_marks().await?,
        before,
        "uncommitted is invisible"
    );
    slow.commit().await?;
    let max_after: i64 = sqlx::query_scalar("SELECT max(version) FROM guild_settings")
        .fetch_one(&db.pool)
        .await?;
    assert!(
        max_after > max_before,
        "the row trigger replaces the stale token"
    );
    let after = store.poll_marks().await?;
    assert_eq!(before.1, after.1);
    assert!(cache.needs_refresh(after.0, after.1));
    let report = cache.refresh(&store.load_snapshot().await?);
    assert!(report.changed);
    assert_eq!(cache.get("g1", KEY), Some(&json!(2)));
    assert_eq!(report.hot.len(), 1);
    assert!(!cache.needs_refresh(after.0, after.1));

    store.set("g1", KEY, None, "delete").await?;
    store
        .set("g1", "TWO_AUTOMOD_REPEAT_COUNT", Some(json!(3)), "replace")
        .await?;
    let marks = store.poll_marks().await?;
    assert_eq!(
        marks.1, after.1,
        "delete plus insert leaves count unchanged"
    );
    assert!(cache.needs_refresh(marks.0, marks.1));
    cache.refresh(&store.load_snapshot().await?);
    assert_eq!(cache.get("g1", KEY), None);
    assert_eq!(cache.get("g1", "TWO_AUTOMOD_REPEAT_COUNT"), Some(&json!(3)));

    let mut rollback = db.pool.begin().await?;
    sqlx::query("DELETE FROM guild_settings")
        .execute(&mut *rollback)
        .await?;
    rollback.rollback().await?;
    assert_eq!(
        store.poll_marks().await?,
        marks,
        "rollback preserves revision"
    );
    db.finish().await
}

async fn concurrent_audit(seed: Option<Value>, changes: [Option<Value>; 2]) -> TestResult {
    let db = TestDb::new().await?;
    let store = SettingsStore::new(&db.pool);
    if let Some(value) = &seed {
        store.set("g1", KEY, Some(value.clone()), "seed").await?;
    }
    let mut gate = db.pool.begin().await?;
    sqlx::query("SELECT revision FROM guild_settings_revision WHERE singleton = TRUE FOR UPDATE")
        .fetch_one(&mut *gate)
        .await?;
    let mut writers = Vec::new();
    for value in changes {
        let pool = db.pool.clone();
        writers.push(tokio::spawn(async move {
            SettingsStore::new(&pool)
                .set("g1", KEY, value, "writer")
                .await
        }));
    }
    db.wait_for_writers(2).await?;
    gate.commit().await?;
    for writer in writers {
        tokio::time::timeout(Duration::from_secs(5), writer).await???;
    }
    let audit = db.audit().await?;
    let offset = usize::from(seed.is_some());
    assert_eq!(audit.len(), offset + 2, "one audit row per write");
    let mut previous = seed;
    for (old, new) in &audit[offset..] {
        assert_eq!(*old, previous, "audit must follow committed transitions");
        previous = new.clone();
    }
    let cache = SettingsCache::load(&store.load_snapshot().await?);
    assert_eq!(cache.get("g1", KEY), previous.as_ref());
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn concurrent_existing_key_writers_audit_the_preceding_commit() -> TestResult {
    concurrent_audit(Some(json!(2)), [Some(json!(3)), Some(json!(4))]).await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn concurrent_absent_key_writers_audit_the_preceding_insert() -> TestResult {
    concurrent_audit(None, [Some(json!(1)), Some(json!(2))]).await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn concurrent_delete_and_upsert_audit_the_preceding_commit() -> TestResult {
    concurrent_audit(Some(json!(2)), [None, Some(json!(4))]).await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn snapshots_pair_rows_and_revision_during_concurrent_writes() -> TestResult {
    let db = TestDb::new().await?;
    let store = SettingsStore::new(&db.pool);
    store.set("g1", KEY, Some(json!(0)), "seed").await?;
    let baseline = store.load_snapshot().await?.revision;
    let pool = db.pool.clone();
    let writer = tokio::spawn(async move {
        for n in 1..=100 {
            // One statement advances the revision once; value and revision
            // must therefore keep this relation in every reader snapshot.
            sqlx::query("UPDATE guild_settings SET value = $1 WHERE key = $2")
                .bind(json!(n))
                .bind(KEY)
                .execute(&pool)
                .await?;
            tokio::task::yield_now().await;
        }
        Ok::<_, sqlx::Error>(())
    });
    for _ in 0..100 {
        let snapshot = store.load_snapshot().await?;
        assert_eq!(snapshot.rows.len(), 1);
        assert_eq!(
            snapshot.rows[0].value.as_i64(),
            Some(snapshot.revision - baseline)
        );
    }
    tokio::time::timeout(Duration::from_secs(5), writer).await???;
    let final_snapshot = store.load_snapshot().await?;
    assert_eq!(final_snapshot.revision, baseline + 100);
    assert_eq!(final_snapshot.rows[0].value, json!(100));
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn audit_is_append_only_and_refusals_write_nothing() -> TestResult {
    let db = TestDb::new().await?;
    let store = SettingsStore::new(&db.pool);
    store.set("g1", KEY, Some(json!(2)), "seed").await?;
    let marks = store.poll_marks().await?;
    for key in ["TWO_MODERATION", "TWO_INTERNAL_FUTURE_GATE", "TWO_UNKNOWN"] {
        assert!(store
            .set("g1", key, Some(json!("fixture")), "writer")
            .await
            .is_err());
    }
    assert!(store.set("g1", KEY, Some(json!(3)), "").await.is_err());
    assert_eq!(store.poll_marks().await?, marks);
    let audit = db.audit().await?;
    assert_eq!(audit, vec![(None, Some(json!(2)))]);
    for statement in [
        "UPDATE guild_settings_audit SET actor = 'rewritten'",
        "DELETE FROM guild_settings_audit",
        "TRUNCATE guild_settings_audit",
        "TRUNCATE guild_settings_audit RESTART IDENTITY CASCADE",
    ] {
        let error = sqlx::query(statement).execute(&db.pool).await.unwrap_err();
        let sqlx::Error::Database(error) = error else {
            panic!("expected database append-only rejection");
        };
        assert_eq!(error.code().as_deref(), Some("P0001"));
        assert!(error.message().contains("append-only"));
        assert_eq!(db.audit().await?, audit);
    }
    db.finish().await
}
