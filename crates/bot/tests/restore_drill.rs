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
async fn successive_drills_allocate_migrate_and_retain_independent_targets() {
    let source = TestDatabase::create(
        "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
        &sqlx::migrate!("../cutover/migrations"),
    )
    .await
    .expect("approved migrated test source");
    sqlx::raw_sql(
        "INSERT INTO moderation_member_bans
           (request_id, guild_id, user_id, generation, state, created_at, completed_at)
         VALUES ('put', 'guild', 'member', 42, 'accepted', NOW(), NOW());
         INSERT INTO moderation_scheduled_unbans
           (request_id, guild_id, user_id, execute_at, reason, state, created_at, claimed_at,
            claim_token, dispatch_uncertain, retry_generation)
         VALUES ('put', 'guild', 'member', NOW(), 'expiry', 'running', NOW(), NOW(),
                 'synthetic-test-claim', TRUE, 999);
         INSERT INTO moderation_warnings (id, guild_id, user_id, actor_id, reason, request_id, created_at)
         VALUES ('warning', 'guild', 'member', 'actor', 'reason', 'warning', NOW());",
    ).execute(source.pool()).await.unwrap();
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
    let bootstrap = "postgres://agent_test:@agent-testdb:5432/postgres";
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
    let first_pool = target_pool(first_name).await;
    let before = evidence(&first_pool).await;
    assert_eq!(before["put_state"], "accepted");
    assert_eq!(before["generation"], 42);
    assert_eq!(before["schedule_state"], "quarantined");
    assert_eq!(before["uncertain"], true);
    assert_eq!(before["token"], "synthetic-test-claim");
    assert_eq!(before["warnings"], 1);
    assert_eq!(first_receipt["quarantined_unbans"], 1);

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
