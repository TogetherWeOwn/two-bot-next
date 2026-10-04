//! Actual recurring-drill CLI against independently allocated disposable DBs.
//! No Discord, staging/production URL, inherited credentials or host service.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, Row};
use tokio::process::Command;
use two_bot_core::backup::dump;
use two_bot_testsupport::TestDatabase;

fn verified_targets(root: &Path) -> Vec<(String, PathBuf, Value)> {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let dir = entry.unwrap().path();
            let receipt: Value = serde_json::from_slice(
                &std::fs::read(dir.join("verified.json")).expect("verified receipt"),
            )
            .unwrap();
            let name = receipt["target_database"].as_str().unwrap().to_owned();
            let suffix = name.strip_prefix("two_next_restore_drill_").unwrap();
            assert_eq!(suffix.len(), 32);
            assert!(suffix
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
            (name, dir, receipt)
        })
        .collect()
}

async fn target_pool(name: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            PgConnectOptions::new_without_pgpass()
                .host("agent-testdb")
                .port(5432)
                .username("agent_test")
                .password("")
                .database(name)
                .ssl_mode(PgSslMode::Disable),
        )
        .await
        .unwrap()
}

async fn evidence(pool: &PgPool) -> Value {
    let row = sqlx::query(
        "SELECT (SELECT state FROM moderation_member_bans WHERE request_id = 'put') AS put_state,
                (SELECT generation FROM moderation_member_bans WHERE request_id = 'put') AS generation,
                (SELECT state FROM moderation_scheduled_unbans WHERE request_id = 'put') AS schedule_state,
                (SELECT dispatch_uncertain FROM moderation_scheduled_unbans WHERE request_id = 'put') AS uncertain,
                (SELECT retry_generation FROM moderation_scheduled_unbans WHERE request_id = 'put') AS retry,
                (SELECT claim_token FROM moderation_scheduled_unbans WHERE request_id = 'put') AS token,
                (SELECT COUNT(*) FROM moderation_warnings) AS warnings",
    ).fetch_one(pool).await.unwrap();
    serde_json::json!({
        "put_state": row.get::<String, _>("put_state"),
        "generation": row.get::<i64, _>("generation"),
        "schedule_state": row.get::<String, _>("schedule_state"),
        "uncertain": row.get::<bool, _>("uncertain"),
        "retry": row.get::<i64, _>("retry"),
        "token": row.get::<String, _>("token"),
        "warnings": row.get::<i64, _>("warnings"),
    })
}

async fn archive_evidence(pool: &PgPool) -> Value {
    let mut evidence = serde_json::Map::new();
    for table in [
        "containment_events",
        "containment_incidents",
        "join_risk_flags",
        "automation_commands",
        "scheduled_messages",
        "automod_violations",
        "automod_processed_messages",
    ] {
        // Only these fixed table identifiers reach SQL; each fixture has one row.
        let rows: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COALESCE(json_agg(row_to_json(t))::text, '[]') FROM {table} t"
        )))
        .fetch_one(pool)
        .await
        .unwrap();
        let rows: Value = serde_json::from_str(&rows).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1, "populated {table}");
        evidence.insert(table.to_owned(), rows);
    }
    Value::Object(evidence)
}

async fn invoke(archive: &Path, root: &Path, bootstrap: &str) -> std::process::Output {
    tokio::time::timeout(
        Duration::from_secs(60),
        Command::new(env!("CARGO_BIN_EXE_two-bot"))
            .args([
                "restore-drill",
                archive.to_str().unwrap(),
                "--confirm-scratch",
            ])
            .env_clear()
            .env("TWO_RESTORE_DRILL_BOOTSTRAP_URL", bootstrap)
            .env("TWO_RESTORE_DRILL_EVIDENCE_DIR", root)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("bounded scratch drill")
    .expect("actual drill binary")
}

#[tokio::test]
#[ignore = "requires explicitly authorized agent-testdb or ephemeral CI service"]
async fn restore_preserves_destination_channel_fence_and_parent() {
    let source_url =
        std::env::var("TWO_TEST_DATABASE_URL").expect("explicit approved source bootstrap");
    let target = TestDatabase::create(&source_url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("approved migrated test target");
    sqlx::raw_sql(include_str!("../src/restore_drill_schema.sql"))
        .execute(target.pool())
        .await
        .unwrap();
    let scratch = PathBuf::from(
        std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .expect("drill tests require run-owned scratch"),
    );
    let dir = scratch.join(format!(
        "channel-restore-test-{:032x}",
        rand::random::<u128>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let archive = dir.join("empty.ndjson.gz");
    dump::dump(target.pool(), &archive).await.unwrap();
    let report = dump::restore(target.pool(), &archive)
        .await
        .expect("fresh restore explicitly includes the empty FK child without CASCADE");
    assert!(report.ok);
    sqlx::raw_sql(
        "INSERT INTO moderation_idempotency
           (guild_id, idempotency_key, action, request_hash, state, claimed_at)
         VALUES ('guild', 'channel-key', 'lockdown', 'synthetic-hash', 'in_flight', NOW());
         INSERT INTO moderation_channel_executions (channel_id, guild_id, idempotency_key, claim_token)
         VALUES ('channel', 'guild', 'channel-key', 'synthetic-fence');",
    ).execute(target.pool()).await.unwrap();
    let refused = dump::restore(target.pool(), &archive)
        .await
        .expect_err("destination history refuses");
    assert!(refused.to_string().contains("moderation history"));
    let fence: (String, String, String, String) = sqlx::query_as(
        "SELECT channel_id, guild_id, idempotency_key, claim_token FROM moderation_channel_executions"
    ).fetch_one(target.pool()).await.unwrap();
    assert_eq!(
        fence,
        (
            "channel".into(),
            "guild".into(),
            "channel-key".into(),
            "synthetic-fence".into()
        )
    );
    let parent: (String, String) = sqlx::query_as(
        "SELECT request_hash, state FROM moderation_idempotency WHERE guild_id = 'guild' AND idempotency_key = 'channel-key'"
    ).fetch_one(target.pool()).await.unwrap();
    assert_eq!(parent, ("synthetic-hash".into(), "in_flight".into()));
    target.close().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
#[ignore = "requires explicitly authorized agent-testdb or ephemeral CI service"]
async fn successive_drills_allocate_migrate_and_retain_independent_targets() {
    let source_url =
        std::env::var("TWO_TEST_DATABASE_URL").expect("explicit approved source bootstrap");
    let source = TestDatabase::create(&source_url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("approved migrated test source");
    sqlx::raw_sql(include_str!("../src/restore_drill_schema.sql"))
        .execute(source.pool())
        .await
        .expect("same shipped scratch archive schema");
    sqlx::raw_sql(
        "INSERT INTO moderation_member_bans
           (request_id, guild_id, user_id, generation, state, created_at, completed_at)
         VALUES ('put', 'guild', 'member', 42, 'accepted', NOW(), NOW());
         INSERT INTO moderation_scheduled_unbans
           (request_id, guild_id, user_id, execute_at, reason, state, created_at, claimed_at,
            claim_token, dispatch_uncertain, retry_generation)
         VALUES ('put', 'guild', 'member', NOW(), 'expiry', 'running', NOW(), NOW(),
                 'synthetic-test-claim', FALSE, 999);
         INSERT INTO moderation_warnings (id, guild_id, user_id, actor_id, reason, request_id, created_at)
         VALUES ('warning', 'guild', 'member', 'actor', 'reason', 'warning', NOW());
         ALTER TABLE moderation_warnings ADD COLUMN archived_note TEXT;
         UPDATE moderation_warnings SET archived_note = 'synthetic archive-only evidence';
         INSERT INTO containment_events
           (audit_entry_id, guild_id, executor_id, action, target_id, weight, occurred_at, state, reason, created_at)
         VALUES ('audit', 'guild', 'actor', 'ban', 'member', 1, '2026-10-01T00:00:00Z', 'observe', 'synthetic', '2026-10-01T00:00:00Z');
         INSERT INTO containment_incidents
           (id, guild_id, executor_id, trigger_audit_entry_id, heat, state, result_json, started_at, cooldown_until, completed_at)
         VALUES ('incident', 'guild', 'actor', 'audit', 1, 'dry_run', '{\"test\":true}', '2026-10-01T00:00:00Z', NULL, NULL);
         INSERT INTO join_risk_flags
           (event_id, guild_id, member_id, account_created_at, joined_at, source, score, reasons_json, bulk_join_window, flagged, created_at)
         VALUES ('join', 'guild', 'member', '2026-09-01T00:00:00Z', '2026-10-01T00:00:00Z', 'synthetic', 1, '[\"test\"]', TRUE, FALSE, '2026-10-01T00:00:00Z');
         INSERT INTO automation_commands
           (guild_id, name, description, template, text_trigger, enabled, created_by, created_at, updated_by, updated_at)
         VALUES ('guild', 'fixture', 'synthetic', 'fixture body', '!fixture', FALSE, 'actor', '2026-10-01T00:00:00Z', 'actor', '2026-10-01T00:00:00Z');
         INSERT INTO scheduled_messages
           (id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled, last_run_at, last_message_id,
            created_by, created_at, updated_by, updated_at, claim_token, claimed_at, occurrence_nonce)
         VALUES ('schedule', 'guild', 'channel', 'synthetic body', '2026-10-02T00:00:00Z', 60, FALSE, NULL, NULL,
                 'actor', '2026-10-01T00:00:00Z', 'actor', '2026-10-01T00:00:00Z', 'synthetic-claim', '2026-10-01T00:00:00Z', 'synthetic-nonce');
         INSERT INTO automod_violations (guild_id, user_id, violation_count, last_filter, last_message_id, updated_at)
         VALUES ('guild', 'member', 2, 'synthetic', 'message', '2026-10-01T00:00:00Z');
         INSERT INTO automod_processed_messages (guild_id, message_id, user_id, processed_at)
         VALUES ('guild', 'message', 'member', '2026-10-01T00:00:00Z');",
    ).execute(source.pool()).await.unwrap();
    let archived = archive_evidence(source.pool()).await;
    let scratch = PathBuf::from(
        std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
            .expect("drill tests require run-owned scratch"),
    );
    let test_dir = scratch.join(format!("drill-test-{:032x}", rand::random::<u128>()));
    std::fs::create_dir_all(&test_dir).unwrap();
    let archive = test_dir.join("moderation.ndjson.gz");
    dump::dump(source.pool(), &archive)
        .await
        .expect("moderation-populated archive");
    let root = test_dir.join("retained-drills");
    std::fs::create_dir(&root).unwrap();
    let bootstrap = "postgres://agent_test:@agent-testdb:5432/postgres";
    let missing_root = test_dir.join("not-provisioned");
    let refused_root = invoke(&archive, &missing_root, bootstrap).await;
    assert_eq!(refused_root.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused_root.stderr).contains("must already exist"));
    assert!(!missing_root.exists());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    let first = invoke(&archive, &root, bootstrap).await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("RESTORE VERIFIED"));
    let targets = verified_targets(&root);
    assert_eq!(targets.len(), 1);
    let (first_name, first_dir, first_receipt) = &targets[0];
    let retained_receipt = std::fs::read(first_dir.join("verified.json")).unwrap();
    let retained_archive = std::fs::read(first_dir.join("archive.ndjson.gz")).unwrap();
    assert_eq!(
        first_receipt["archive"]["sha256"],
        two_bot_core::backup::s3::sha256_hex(&retained_archive)
    );
    assert_eq!(
        first_receipt["archive"]["bytes"],
        retained_archive.len() as u64
    );
    let first_pool = target_pool(first_name).await;
    let before = evidence(&first_pool).await;
    assert_eq!(before["put_state"], "accepted");
    assert_eq!(before["generation"], 42);
    assert_eq!(before["schedule_state"], "quarantined");
    assert_eq!(before["uncertain"], true);
    assert_eq!(before["token"], "synthetic-test-claim");
    assert_eq!(before["warnings"], 1);
    assert_eq!(first_receipt["quarantined_unbans"], 1);
    assert_eq!(archive_evidence(&first_pool).await, archived);
    assert_eq!(
        first_receipt["dropped_columns"],
        serde_json::json!({"moderation_warnings": ["archived_note"]})
    );
    assert!(String::from_utf8_lossy(&first.stderr).contains("archive columns were dropped"));
    let empty_fences: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM moderation_channel_executions")
            .fetch_one(&first_pool)
            .await
            .unwrap();
    assert_eq!(
        empty_fences, 0,
        "fresh restore tolerates the empty FK child explicitly"
    );

    let second = invoke(&archive, &root, bootstrap).await;
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let targets = verified_targets(&root);
    assert_eq!(targets.len(), 2);
    let (second_name, _, second_receipt) = targets
        .iter()
        .find(|(name, _, _)| name != first_name)
        .unwrap();
    let second_pool = target_pool(second_name).await;
    assert_eq!(evidence(&second_pool).await, before);
    assert_eq!(archive_evidence(&second_pool).await, archived);
    assert_eq!(
        second_receipt["dropped_columns"],
        first_receipt["dropped_columns"]
    );
    assert_eq!(
        evidence(&first_pool).await,
        before,
        "second drill leaves first target untouched"
    );
    assert_eq!(first_receipt["archive"], second_receipt["archive"]);
    assert_eq!(
        std::fs::read(first_dir.join("verified.json")).unwrap(),
        retained_receipt
    );
    assert_eq!(
        std::fs::read(first_dir.join("archive.ndjson.gz")).unwrap(),
        retained_archive
    );
    let next: i64 = sqlx::query_scalar("SELECT nextval('moderation_member_bans_generation_seq')")
        .fetch_one(&second_pool)
        .await
        .unwrap();
    assert_eq!(
        next, 1000,
        "real migrated restore includes retry-ticket high water"
    );
    let refused = dump::restore(&first_pool, &archive)
        .await
        .expect_err("history remains protected");
    assert!(refused.to_string().contains("moderation history"));
    assert_eq!(evidence(&first_pool).await, before);

    let invalid = invoke(
        &archive,
        &root,
        "postgres://agent_test:SECRET_SENTINEL@production:5432/postgres",
    )
    .await;
    assert_eq!(invalid.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&invalid.stderr).contains("SECRET_SENTINEL"));
    assert_eq!(
        verified_targets(&root).len(),
        2,
        "invalid authority allocates nothing"
    );
    first_pool.close().await;
    second_pool.close().await;
    let admin = target_pool("postgres").await;
    for (name, _, _) in targets {
        // Only the two generated test-owned targets verified above are removed.
        // The shipped allocator never drops databases or prunes drill evidence.
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE \"{name}\"")))
            .execute(&admin)
            .await
            .unwrap();
    }
    admin.close().await;
    source.close().await.unwrap();
    std::fs::remove_dir_all(test_dir).unwrap();
}
