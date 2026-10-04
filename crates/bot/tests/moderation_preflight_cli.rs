//! `two-bot moderation preflight` operator acceptance plus the boot-refusal
//! startup test. Real binary, disposable databases only: every
//! case mints its own migrated database through `TestDatabase` and drops it
//! on close. No gateway, Discord, or staging/production contact.

use std::process::Output;
use std::time::Duration;
use tokio::process::Command;
use two_bot_testsupport::TestDatabase;

async fn cli(args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command
        .env_clear()
        .env("TWO_DATABASE_TLS", "local-only")
        .args(args)
        .kill_on_drop(true);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("moderation preflight must exit, not start the gateway")
        .unwrap()
}

fn bootstrap() -> Option<String> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure moderation preflight acceptance"
            );
            eprintln!("SKIP moderation preflight CLI acceptance: TWO_TEST_DATABASE_URL not set");
            None
        }
    }
}

async fn migrated() -> Option<TestDatabase> {
    Some(
        TestDatabase::create(&bootstrap()?, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated test database"),
    )
}

fn db_url(db: &TestDatabase) -> String {
    format!("postgres://agent_test:@agent-testdb:5432/{}", db.name())
}

async fn seed_owed(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO moderation_scheduled_unbans
           (request_id, guild_id, user_id, execute_at, reason, state, created_at)
         VALUES ('req-cli-owed', 'g1', 'u1', '2026-02-01T00:00:00Z',
                 'tempban', 'pending', '2026-01-01T00:00:00Z')",
    )
    .execute(pool)
    .await
    .expect("seeds an owed unban");
    sqlx::query(
        "INSERT INTO moderation_lockdowns
           (channel_id, guild_id, prior_allow, prior_deny, prior_exists, reason, locked_at)
         VALUES ('chan-cli-owed', 'g1', '0', '0', FALSE, 'raid', '2026-01-01T00:00:00Z')",
    )
    .execute(pool)
    .await
    .expect("seeds an owed lockdown");
    sqlx::query(
        "INSERT INTO scheduled_messages
           (id, guild_id, channel_id, body, next_run_at, enabled,
            created_by, created_at, updated_by, updated_at)
         VALUES ('msg-cli-owed', 'g1', 'c1', 'hello', '2026-02-01T00:00:00.000Z', TRUE,
                 'op', '2026-01-01T00:00:00.000Z', 'op', '2026-01-01T00:00:00.000Z')",
    )
    .execute(pool)
    .await
    .expect("seeds an owed schedule");
}

#[tokio::test]
async fn moderation_help_names_the_subcommand() {
    for args in [
        &["moderation", "--help"][..],
        &["moderation", "preflight", "--help"][..],
    ] {
        let help = cli(args, &[]).await;
        assert_eq!(help.status.code(), Some(0));
        let text = String::from_utf8_lossy(&help.stdout);
        assert!(
            text.contains("two-bot moderation preflight"),
            "{args:?}: {text}"
        );
    }
}

#[tokio::test]
async fn refuses_owed_releases_naming_ids() {
    let Some(db) = migrated().await else { return };
    seed_owed(db.pool()).await;
    let url = db_url(&db);
    let output = cli(&["moderation", "preflight"], &[("TWO_DATABASE_URL", &url)]).await;
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8_lossy(&output.stdout);
    for id in ["req-cli-owed", "chan-cli-owed", "msg-cli-owed"] {
        assert!(text.contains(id), "refusal names {id}: {text}");
    }
    assert!(text.contains("REFUSED"), "{text}");
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn override_flag_allows_owed_loudly() {
    let Some(db) = migrated().await else { return };
    seed_owed(db.pool()).await;
    let url = db_url(&db);
    let output = cli(
        &["moderation", "preflight", "--allow-owed"],
        &[("TWO_DATABASE_URL", &url)],
    )
    .await;
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("OVERRIDE"), "{text}");
    assert!(
        text.contains("req-cli-owed"),
        "override still reports ids: {text}"
    );
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn env_override_exits_zero_but_json_still_reports_owed_releases() {
    let Some(db) = migrated().await else { return };
    seed_owed(db.pool()).await;
    let url = db_url(&db);
    let output = cli(
        &["moderation", "preflight", "--json"],
        &[("TWO_DATABASE_URL", &url), ("TWO_ALLOW_OWED_RELEASES", "1")],
    )
    .await;
    assert_eq!(output.status.code(), Some(0));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("stdout is JSON");
    assert_eq!(report["clear"], false);
    assert_eq!(report["overridden"], true);
    assert_eq!(
        report["pending_unbans"],
        serde_json::json!(["req-cli-owed"])
    );

    // The recovery verification command must omit both overrides, not trust exit 0.
    let output = cli(
        &["moderation", "preflight", "--json"],
        &[("TWO_DATABASE_URL", &url)],
    )
    .await;
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("stdout is JSON");
    assert_eq!(report["clear"], false);
    assert_eq!(report["overridden"], false);
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn clear_database_exits_zero() {
    let Some(db) = migrated().await else { return };
    let url = db_url(&db);
    let output = cli(&["moderation", "preflight"], &[("TWO_DATABASE_URL", &url)]).await;
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("CLEAR"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn json_reports_owed_sections() {
    let Some(db) = migrated().await else { return };
    seed_owed(db.pool()).await;
    let url = db_url(&db);
    let output = cli(
        &["moderation", "preflight", "--json"],
        &[("TWO_DATABASE_URL", &url)],
    )
    .await;
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("stdout is JSON");
    assert_eq!(report["clear"], false);
    assert_eq!(
        report["pending_unbans"],
        serde_json::json!(["req-cli-owed"])
    );
    assert_eq!(
        report["active_lockdowns"],
        serde_json::json!(["chan-cli-owed"])
    );
    assert_eq!(
        report["enabled_scheduled"],
        serde_json::json!(["msg-cli-owed"])
    );
    db.close().await.expect("drops fixture database");
}

async fn seed_unban(pool: &sqlx::PgPool, request: &str, state: &str) {
    sqlx::query(
        "INSERT INTO moderation_scheduled_unbans
           (request_id, guild_id, user_id, execute_at, reason, state, created_at)
         VALUES ($1, 'g1', 'u1', '2026-02-01T00:00:00Z', 'tempban', $2, '2026-01-01T00:00:00Z')",
    )
    .bind(request)
    .bind(state)
    .execute(pool)
    .await
    .expect("seeds a scheduled unban");
}

#[tokio::test]
async fn stranded_running_unban_is_tagged_in_text_and_json_without_changing_exit_codes() {
    let Some(db) = migrated().await else { return };
    seed_unban(db.pool(), "req-cli-stranded", "running").await;
    seed_unban(db.pool(), "req-cli-waiting", "pending").await;
    let url = db_url(&db);

    let text = cli(&["moderation", "preflight"], &[("TWO_DATABASE_URL", &url)]).await;
    assert_eq!(text.status.code(), Some(1));
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(
        text.contains("1 running unban claim(s) [running]: req-cli-stranded"),
        "{text}"
    );
    assert!(
        text.contains("1 pending unban(s): req-cli-waiting"),
        "{text}"
    );
    assert!(
        text.contains("docs/moderation-disable-preflight.md#stranded-running-unban-claims"),
        "{text}"
    );

    let json = cli(
        &["moderation", "preflight", "--json"],
        &[("TWO_DATABASE_URL", &url)],
    )
    .await;
    assert_eq!(json.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).expect("stdout is JSON");
    assert_eq!(report["schema_version"], 1);
    // Existing key keeps every owed unban; the new key is the running subset.
    assert_eq!(
        report["pending_unbans"],
        serde_json::json!(["req-cli-stranded", "req-cli-waiting"])
    );
    assert_eq!(
        report["running_unbans"],
        serde_json::json!(["req-cli-stranded"])
    );

    let overridden = cli(
        &["moderation", "preflight", "--allow-owed"],
        &[("TWO_DATABASE_URL", &url)],
    )
    .await;
    assert_eq!(overridden.status.code(), Some(0));
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn pending_only_cli_refusal_carries_no_running_tag() {
    let Some(db) = migrated().await else { return };
    seed_owed(db.pool()).await;
    let url = db_url(&db);
    let text = cli(&["moderation", "preflight"], &[("TWO_DATABASE_URL", &url)]).await;
    assert_eq!(text.status.code(), Some(1));
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(!text.contains("[running]"), "{text}");
    assert!(!text.contains("stranded-running-unban-claims"), "{text}");
    let json = cli(
        &["moderation", "preflight", "--json"],
        &[("TWO_DATABASE_URL", &url)],
    )
    .await;
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).expect("stdout is JSON");
    assert_eq!(report["running_unbans"], serde_json::json!([]));
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn unreadable_state_is_unknown_not_refused() {
    // No database is contacted: an invalid URL cannot be confused with owed.
    let output = cli(
        &["moderation", "preflight"],
        &[("TWO_DATABASE_URL", "not-a-postgres-url")],
    )
    .await;
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("UNKNOWN"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let missing = cli(&["moderation", "preflight"], &[]).await;
    assert_eq!(missing.status.code(), Some(2));
}

#[tokio::test]
async fn boot_refuses_with_disabled_moderation_while_unban_owed() {
    let Some(db) = migrated().await else { return };
    seed_owed(db.pool()).await;
    let url = db_url(&db);
    // Full boot: moderation/automation gates default off, database present.
    // The disable guard must stop the container before the gateway starts.
    let output = cli(
        &[],
        &[
            ("DISCORD_TOKEN", "fixture-token"),
            ("DATABASE_URL", &url),
            ("GUILD_ID", "123"),
            ("LISTEN_ADDR", "127.0.0.1:0"),
            ("TWO_DATABASE_TLS", "local-only"),
        ],
    )
    .await;
    assert_eq!(output.status.code(), Some(1));
    // The runtime subscriber writes tracing records to stdout, so both
    // streams count as the boot log here.
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(logs.contains("moderation_disable_refused"), "{logs}");
    assert!(
        logs.contains("req-cli-owed"),
        "boot refusal names ids: {logs}"
    );
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn boot_proceeds_past_guard_when_nothing_owed() {
    let Some(db) = migrated().await else { return };
    // Nothing owed: the guard passes and boot continues toward the gateway.
    // The only assertion is that the disable guard never refused — a later
    // gateway failure with the fixture token still passes, because it proves
    // boot got past the guard.
    let mut command = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    let url = db_url(&db);
    command
        .env_clear()
        .env("TWO_DATABASE_TLS", "local-only")
        .env("DISCORD_TOKEN", "fixture-token")
        .env("DATABASE_URL", url)
        .env("GUILD_ID", "123")
        .env("LISTEN_ADDR", "127.0.0.1:0")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("spawns the bot");
    // The guard runs before the gateway task; a refusal exits within seconds
    // while a clean boot stays alive dialling out. Either way, stop waiting
    // after 12 s and judge by the logs, not the exit code.
    for _ in 0..60 {
        if child.try_wait().expect("polls the child").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    child.kill().await.ok();
    let output = child
        .wait_with_output()
        .await
        .expect("reaps the booted bot");
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !logs.contains("moderation_disable_refused"),
        "boot must not refuse with nothing owed: {logs}"
    );
    db.close().await.expect("drops fixture database");
}
