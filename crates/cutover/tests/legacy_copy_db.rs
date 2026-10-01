//! Two owned scratch DATABASES on agent-testdb only. Never read an app URL.
//! cargo test -p two-bot-cutover --test legacy_copy_db --locked -- --ignored
use serde_json::Value;
use sqlx::postgres::{PgPoolOptions, PgSslMode};
use sqlx::{PgPool, Row};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use two_bot_cutover::legacy_copy::{copy, mapping, options::guarded_target, CopyError, Table};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

struct Pair {
    admin: PgPool,
    source: PgPool,
    target: PgPool,
    names: [String; 2],
}

impl Pair {
    async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let suffix = format!(
            "{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        let names = [
            format!("two_bot_test_copy_s_{suffix}"),
            format!("two_bot_test_copy_t_{suffix}"),
        ];
        // Same fail-closed parsed-options guard as the copier. CI resolves this
        // exact hostname to its service in the existing prepare-DB step.
        let options = guarded_target(
            &format!("postgres://agent_test:@agent-testdb:5432/{}", names[0]),
            false,
        )?
        .ssl_mode(PgSslMode::Disable)
        .options([("timezone", "UTC"), ("statement_timeout", "10000")]);
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.clone().database("postgres"))
            .await?;
        for name in &names {
            sqlx::query(database_sql("CREATE DATABASE", name))
                .execute(&admin)
                .await?;
        }
        let source = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.clone().database(&names[0]))
            .await?;
        let target = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options.database(&names[1]))
            .await?;
        Ok(Self {
            admin,
            source,
            target,
            names,
        })
    }

    async fn cleanup(self) -> TestResult {
        self.source.close().await;
        self.target.close().await;
        for name in &self.names {
            sqlx::query(database_sql("DROP DATABASE", name))
                .execute(&self.admin)
                .await?;
        }
        self.admin.close().await;
        Ok(())
    }
}

fn database_sql(operation: &str, name: &str) -> sqlx::AssertSqlSafe<String> {
    assert!(name.starts_with("two_bot_test_copy_") && name.len() <= 63);
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    sqlx::AssertSqlSafe(format!("{operation} \"{name}\""))
}

fn projection(table: &Table, source: bool) -> sqlx::AssertSqlSafe<String> {
    // Compiled mapping identifiers only. Compare every mapped value in native
    // target types, preserving timestamp precision, NULL, JSONB and big IDs.
    let fields = table
        .columns
        .iter()
        .map(|c| {
            let column = if source { c.source } else { c.target };
            format!("'{0}', \"{column}\"::{1}", c.target, c.pg_type)
        })
        .collect::<Vec<_>>()
        .join(", ");
    let keys = if source { table.keys } else { table.conflict }
        .iter()
        .map(|k| format!("\"{k}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let name = if source { table.source } else { table.target };
    sqlx::AssertSqlSafe(format!(
        "SELECT jsonb_build_object({fields}) FROM public.\"{name}\" ORDER BY {keys}"
    ))
}

async fn values(pool: &PgPool, table: &Table, source: bool) -> Result<Vec<Value>, sqlx::Error> {
    sqlx::query_scalar(projection(table, source))
        .fetch_all(pool)
        .await
}

async fn initialize(source: &PgPool, target: &PgPool) -> TestResult {
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy_migrations");
    let mut paths: Vec<_> = std::fs::read_dir(fixture)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    paths.retain(|p| p.extension().is_some_and(|e| e == "sql"));
    paths.sort();
    assert_eq!(
        paths.len(),
        54,
        "the pinned 0001–0042 set has repeated prefixes"
    );
    for path in paths {
        // Pinned, vendored SQL files only; no operator-supplied path or SQL.
        sqlx::raw_sql(sqlx::AssertSqlSafe(std::fs::read_to_string(&path)?))
            .execute(source)
            .await
            .map_err(|error| format!("{}: {error}", path.file_name().unwrap().to_string_lossy()))?;
    }
    // Includes the additive 0390 preservation of legacy 0041 scan evidence.
    sqlx::migrate!("./migrations").run(target).await?;
    sqlx::raw_sql(include_str!("fixtures/legacy_copy_seed.sql"))
        .execute(source)
        .await?;
    Ok(())
}

async fn scenarios(source: PgPool, target: PgPool) -> TestResult {
    initialize(&source, &target).await?;
    let tables = mapping::select(&["ready".to_owned()])?;
    assert_eq!(tables.len(), 25);
    let before: Vec<_> = {
        let mut rows = Vec::new();
        for table in &tables {
            rows.push(values(&target, table, false).await?);
        }
        rows
    };
    // Dry-run counts all tables but must not mutate seed rows, revisions or sequences.
    let dry = copy(&source, &target, &tables, 1, false).await?;
    for (table, receipt) in tables.iter().zip(&dry) {
        assert!(receipt.source_rows > 0, "{} fixture is empty", table.source);
        assert_eq!(receipt.changed, 0);
        assert_eq!(receipt.batches, 0);
    }
    for (table, rows) in tables.iter().zip(&before) {
        assert_eq!(&values(&target, table, false).await?, rows);
    }
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM guild_settings_revision")
        .fetch_one(&target)
        .await?;
    assert_eq!(revision, 0);
    let sequence_called: bool = sqlx::query_scalar("SELECT is_called FROM events_id_seq")
        .fetch_one(&target)
        .await?;
    assert!(!sequence_called);

    // Force a genuine interruption AFTER the first batch committed. The second
    // batch errors inside Postgres, not through a mock or a private test hook.
    sqlx::raw_sql("CREATE FUNCTION copy_interrupt() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.id = 10 THEN RAISE EXCEPTION 'synthetic interruption'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER copy_interrupt BEFORE INSERT ON events FOR EACH ROW EXECUTE FUNCTION copy_interrupt();")
        .execute(&target).await?;
    let events = tables.iter().find(|t| t.source == "events").unwrap();
    assert!(matches!(
        copy(&source, &target, &[*events], 1, true).await,
        Err(CopyError::Database { .. })
    ));
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM events ORDER BY id")
        .fetch_all(&target)
        .await?;
    assert_eq!(ids, vec![2], "native numeric keyset starts with 2, not 10");
    let first_xmin: String = sqlx::query_scalar("SELECT xmin::text FROM events WHERE id=2")
        .fetch_one(&target)
        .await?;
    sqlx::raw_sql("DROP TRIGGER copy_interrupt ON events; DROP FUNCTION copy_interrupt();")
        .execute(&target)
        .await?;
    let resumed = copy(&source, &target, &[*events], 1, true).await?;
    assert_eq!(resumed[0].scanned, 3);
    assert_eq!(resumed[0].changed, 2);
    assert_eq!(resumed[0].batches, 3);
    let first_xmin_after: String = sqlx::query_scalar("SELECT xmin::text FROM events WHERE id=2")
        .fetch_one(&target)
        .await?;
    assert_eq!(
        first_xmin, first_xmin_after,
        "committed batch was not rewritten"
    );

    let receipts = copy(&source, &target, &tables, 2, true).await?;
    assert!(receipts.iter().any(|r| r.changed > 0));
    for table in &tables {
        assert_eq!(
            values(&source, table, true).await?,
            values(&target, table, false).await?,
            "all mapped columns of {}",
            table.source
        );
    }
    let revision_after: i64 = sqlx::query_scalar("SELECT revision FROM guild_settings_revision")
        .fetch_one(&target)
        .await?;
    assert!(revision_after > revision);
    let settings_metadata: Vec<(String, i64)> =
        sqlx::query_as("SELECT xmin::text, cas_version FROM guild_settings ORDER BY guild_id, key")
            .fetch_all(&target)
            .await?;
    let cas_allocation: i64 = sqlx::query_scalar("SELECT last_value FROM guild_settings_cas_seq")
        .fetch_one(&target)
        .await?;
    let replay = copy(&source, &target, &tables, 1, true).await?;
    assert!(replay.iter().all(|r| r.changed == 0));
    assert_eq!(
        sqlx::query_as::<_, (String, i64)>(
            "SELECT xmin::text, cas_version FROM guild_settings ORDER BY guild_id, key",
        )
        .fetch_all(&target)
        .await?,
        settings_metadata,
        "no-op copy preserves tuple identity and destination CAS tokens"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT last_value FROM guild_settings_cas_seq")
            .fetch_one(&target)
            .await?,
        cas_allocation,
        "no-op copy does not allocate CAS tokens"
    );
    let revision_replay: i64 = sqlx::query_scalar("SELECT revision FROM guild_settings_revision")
        .fetch_one(&target)
        .await?;
    assert_eq!(
        revision_after, revision_replay,
        "no-op replay issues no settings DML"
    );

    // A genuine import change must invalidate destination CAS even when the
    // source retains the SAME legacy version. Preserve the existing mapping;
    // do not weaken equality/parity checks to accommodate rewritten versions.
    let settings = tables
        .iter()
        .find(|t| t.source == "guild_settings")
        .unwrap();
    let old_token: i64 = sqlx::query_scalar(
        "SELECT cas_version FROM guild_settings WHERE guild_id='g1' AND key='TWO_RAID_JOIN_THRESHOLD'",
    )
    .fetch_one(&target)
    .await?;
    sqlx::query(
        "UPDATE guild_settings SET value='4' WHERE guild_id='g1' AND key='TWO_RAID_JOIN_THRESHOLD'",
    )
    .execute(&source)
    .await?;
    let changed = copy(&source, &target, &[*settings], 1, true).await?;
    assert_eq!(changed[0].changed, 1);
    assert_eq!(
        values(&source, settings, true).await?,
        values(&target, settings, false).await?,
        "genuine change preserves every mapped legacy column"
    );
    let (legacy_version, new_token): (i64, i64) = sqlx::query_as(
        "SELECT version, cas_version FROM guild_settings WHERE guild_id='g1' AND key='TWO_RAID_JOIN_THRESHOLD'",
    )
    .fetch_one(&target)
    .await?;
    assert_eq!(legacy_version, 19);
    assert!(new_token < old_token);
    let store = two_bot_cutover::settings::SettingsStore::new(&target);
    let marks = store.poll_marks().await?;
    for value in [Some(serde_json::json!(5)), None] {
        assert!(matches!(
            store
                .set_if_version(
                    "g1",
                    "TWO_RAID_JOIN_THRESHOLD",
                    value,
                    "fixture",
                    Some(old_token)
                )
                .await,
            Err(two_bot_cutover::settings::SettingsWriteError::VersionConflict)
        ));
    }
    assert_eq!(store.poll_marks().await?, marks);
    assert_eq!(
        store.get("g1", "TWO_RAID_JOIN_THRESHOLD").await?,
        Some((serde_json::json!(4), new_token))
    );
    assert_eq!(
        copy(&source, &target, &[*settings], 1, true).await?[0].changed,
        0
    );

    // Every preserved serial/version allocator must advance past copied IDs.
    for table in &tables {
        if let Some(sequence) = table.sequence {
            let maximum: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT max(\"{}\") FROM public.\"{}\"",
                sequence.column, table.target
            )))
            .fetch_one(&target)
            .await?;
            let next: i64 = sqlx::query_scalar("SELECT nextval($1::regclass)")
                .bind(format!("public.{}", sequence.name))
                .fetch_one(&target)
                .await?;
            assert!(next > maximum, "{} was not reconciled", sequence.name);
        }
    }
    let high = sqlx::query_scalar::<_, i64>("SELECT last_value FROM events_id_seq")
        .fetch_one(&target)
        .await?;
    copy(&source, &target, &[*events], 1, true).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT last_value FROM events_id_seq")
            .fetch_one(&target)
            .await?,
        high,
        "replay cannot lower sequences"
    );

    // Append-only: an equal replay is allowed, a divergent audit row refuses
    // rather than updating, silently ignoring, or disabling the target trigger.
    let audit = tables
        .iter()
        .find(|t| t.source == "guild_settings_audit")
        .unwrap();
    sqlx::query("DROP TRIGGER trg_guild_settings_audit_append_only ON guild_settings_audit")
        .execute(&source)
        .await?;
    sqlx::query("UPDATE guild_settings_audit SET actor='changed fixture' WHERE id=10")
        .execute(&source)
        .await?;
    assert!(matches!(
        copy(&source, &target, &[*audit], 1, true).await,
        Err(CopyError::AppendOnlyConflict { .. })
    ));
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT actor FROM guild_settings_audit WHERE id=10")
            .fetch_one(&target)
            .await?,
        "fixture"
    );
    assert!(
        sqlx::query("UPDATE guild_settings_audit SET actor='must fail'")
            .execute(&target)
            .await
            .is_err()
    );

    // Selected schema preflight and unresolved delivery validation occur before
    // any target DML even when the bad table comes after a changed source row.
    sqlx::query("UPDATE events SET source='updated fixture' WHERE id=2")
        .execute(&source)
        .await?;
    sqlx::query(
        "UPDATE operational_audit_log SET delivery_state='pending' WHERE entry_id='audit2'",
    )
    .execute(&source)
    .await?;
    let delivery = tables
        .iter()
        .find(|t| t.source == "operational_audit_log")
        .unwrap();
    assert!(matches!(
        copy(&source, &target, &[*events, *delivery], 1, true).await,
        Err(CopyError::UnresolvedDelivery { .. })
    ));
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT source FROM events WHERE id=2")
            .fetch_one(&target)
            .await?,
        "fixture"
    );
    let broken = Table {
        target: "missing_fixture_target",
        ..*events
    };
    assert!(copy(&source, &target, &[*events, broken], 1, true)
        .await
        .is_err());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT source FROM events WHERE id=2")
            .fetch_one(&target)
            .await?,
        "fixture"
    );

    // Ordinary changed rows upsert; alternate unique-key collisions fail and do
    // not overwrite an unrelated primary-key row.
    assert_eq!(
        copy(&source, &target, &[*events], 1, true).await?[0].changed,
        1
    );
    sqlx::query("UPDATE events SET idempotency_key='target-only-key' WHERE id=10")
        .execute(&target)
        .await?;
    sqlx::query("INSERT INTO events (id,event_type,guild_id,occurred_at,recorded_at,source,idempotency_key) VALUES (11,'landing_viewed','g1',now(),now(),'target','event10')").execute(&target).await?;
    assert!(copy(&source, &target, &[*events], 1, true).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT source FROM events WHERE id=11")
            .fetch_one(&target)
            .await?,
        "target"
    );

    // New target-only audit fields keep defaults (not NULL), with no delivery
    // work enqueued; administrative halt evidence is preserved.
    let row = sqlx::query("SELECT delivery_generation, delivery_accepted_at FROM operational_audit_log WHERE entry_id='audit1'").fetch_one(&target).await?;
    assert_eq!(row.try_get::<i64, _>("delivery_generation")?, 0);
    assert!(row
        .try_get::<Option<time::OffsetDateTime>, _>("delivery_accepted_at")?
        .is_none());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_kill_switch")
            .fetch_one(&target)
            .await?,
        1
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb or the CI service, never staging/production"]
async fn two_database_copy_acceptance() -> TestResult {
    let pair = Pair::new().await?;
    let source = pair.source.clone();
    let target = pair.target.clone();
    // Ensure cleanup also runs when an assertion panics in the scenario task.
    let result = tokio::spawn(scenarios(source, target)).await;
    pair.cleanup().await?;
    result??;
    Ok(())
}
