//! Real-binary argument-routing acceptance for the documented bare-boot
//! `--allow-owed` override.
//!
//! The disable guard already accepts `--allow-owed` (and
//! `TWO_ALLOW_OWED_RELEASES=1`) when it evaluates owed releases at boot,
//! and `two-bot moderation preflight --allow-owed` already works. The
//! missing behaviour pinned here is narrower: the bare service-start
//! invocation `two-bot --allow-owed` must reach that existing boot
//! preflight instead of being rejected by the backup unknown-subcommand
//! handler. Only the exact flag may be recognized: misspellings, unknown
//! arguments (alone or beside the exact flag) and operator subcommands
//! carrying the flag must keep exit-2 usage errors, and the default
//! no-override refusal must stay byte-for-byte the existing one.
//!
//! Disposable databases only: every database case mints its own migrated
//! database through `TestDatabase` and drops it on close. No gateway,
//! Discord, or staging/production contact.

use std::process::Output;
use std::time::Duration;
use tokio::process::Command;
use two_bot_testsupport::TestDatabase;

/// Run the binary to completion (cases that must exit on their own).
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
        .expect("invocation must exit, not start the gateway")
        .unwrap()
}

/// Spawn a bare boot, give the pre-gateway disable guard time to run,
/// then stop the child and return (exit code if it exited alone, logs).
/// Judged by logs, mirroring `boot_proceeds_past_guard_when_nothing_owed`:
/// a boot that passed the guard stays alive dialling the gateway with the
/// fixture token, a refused boot exits 1 within seconds, and a routing
/// rejection exits 2 immediately.
async fn boot(args: &[&str], extra_env: &[(&str, &str)]) -> (Option<i32>, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command
        .env_clear()
        .env("TWO_DATABASE_TLS", "local-only")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("spawns the bot");
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
    (output.status.code(), logs)
}

fn bootstrap() -> Option<String> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure allow-owed boot acceptance"
            );
            eprintln!("SKIP allow-owed boot acceptance: TWO_TEST_DATABASE_URL not set");
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

/// Full-boot environment: moderation/automation gates default off, a
/// disposable database is present, and the token is a fixture, so boot
/// never contacts Discord before the disable guard has run.
fn boot_env(url: &str) -> [(&str, &str); 4] {
    [
        ("DISCORD_TOKEN", "fixture-token"),
        ("DATABASE_URL", url),
        ("GUILD_ID", "123"),
        ("LISTEN_ADDR", "127.0.0.1:0"),
    ]
}

async fn seed_owed(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO moderation_scheduled_unbans
           (request_id, guild_id, user_id, execute_at, reason, state, created_at)
         VALUES ('req-flag-owed', 'g1', 'u1', '2026-02-01T00:00:00Z',
                 'tempban', 'pending', '2026-01-01T00:00:00Z')",
    )
    .execute(pool)
    .await
    .expect("seeds an owed unban");
    sqlx::query(
        "INSERT INTO moderation_lockdowns
           (channel_id, guild_id, prior_allow, prior_deny, prior_exists, reason, locked_at)
         VALUES ('chan-flag-owed', 'g1', '0', '0', FALSE, 'raid', '2026-01-01T00:00:00Z')",
    )
    .execute(pool)
    .await
    .expect("seeds an owed lockdown");
    sqlx::query(
        "INSERT INTO scheduled_messages
           (id, guild_id, channel_id, body, next_run_at, enabled,
            created_by, created_at, updated_by, updated_at)
         VALUES ('msg-flag-owed', 'g1', 'c1', 'hello', '2026-02-01T00:00:00.000Z', TRUE,
                 'op', '2026-01-01T00:00:00.000Z', 'op', '2026-01-01T00:00:00.000Z')",
    )
    .execute(pool)
    .await
    .expect("seeds an owed schedule");
}

// ---------------------------------------------------------------------------
// The missing behaviour (RED until the flag is routed to the boot preflight)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bare_boot_exact_allow_owed_reaches_preflight_and_overrides_loudly() {
    let Some(db) = migrated().await else { return };
    seed_owed(db.pool()).await;
    let url = db_url(&db);
    let (code, logs) = boot(&["--allow-owed"], &boot_env(&url)).await;
    assert_ne!(
        code,
        Some(2),
        "exact flag must not be a usage error: {logs}"
    );
    assert!(
        !logs.contains("unknown subcommand"),
        "exact flag must not reach the backup unknown-subcommand handler: {logs}"
    );
    assert!(
        logs.contains("moderation_disable_override"),
        "exact flag must reach the boot preflight override warning: {logs}"
    );
    assert!(
        logs.contains("req-flag-owed"),
        "override warning still names the owed ids: {logs}"
    );
    assert!(
        !logs.contains("moderation_disable_refused"),
        "overridden boot must not also refuse: {logs}"
    );
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn bare_boot_exact_allow_owed_with_nothing_owed_proceeds_past_guard() {
    let Some(db) = migrated().await else { return };
    let url = db_url(&db);
    let (code, logs) = boot(&["--allow-owed"], &boot_env(&url)).await;
    assert_ne!(
        code,
        Some(2),
        "exact flag must not be a usage error: {logs}"
    );
    assert!(
        !logs.contains("unknown subcommand"),
        "exact flag must not reach the backup unknown-subcommand handler: {logs}"
    );
    assert!(
        !logs.contains("moderation_disable_refused"),
        "boot must not refuse with nothing owed: {logs}"
    );
    assert!(
        !logs.contains("moderation_disable_override"),
        "override warning is only for owed releases, not a clear boot: {logs}"
    );
    db.close().await.expect("drops fixture database");
}

// ---------------------------------------------------------------------------
// Controls: existing behaviour the routing fix must not change
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bare_boot_without_override_still_refuses_owed_releases() {
    let Some(db) = migrated().await else { return };
    seed_owed(db.pool()).await;
    let url = db_url(&db);
    let output = cli(&[], &boot_env(&url)).await;
    assert_eq!(output.status.code(), Some(1));
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(logs.contains("moderation_disable_refused"), "{logs}");
    assert!(
        logs.contains("req-flag-owed"),
        "boot refusal names ids: {logs}"
    );
    assert!(
        logs.contains("TWO_ALLOW_OWED_RELEASES=1"),
        "refusal keeps naming the explicit override: {logs}"
    );
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn bare_boot_env_override_still_proceeds_with_warning() {
    let Some(db) = migrated().await else { return };
    seed_owed(db.pool()).await;
    let url = db_url(&db);
    let mut env = boot_env(&url).to_vec();
    env.push(("TWO_ALLOW_OWED_RELEASES", "1"));
    let (code, logs) = boot(&[], &env).await;
    assert_ne!(
        code,
        Some(2),
        "env override boot is not a usage error: {logs}"
    );
    assert!(
        logs.contains("moderation_disable_override"),
        "env override keeps the existing loud warning: {logs}"
    );
    assert!(
        !logs.contains("moderation_disable_refused"),
        "overridden boot must not also refuse: {logs}"
    );
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn misspellings_and_unknown_arguments_still_exit_2() {
    // Routing is decided before any env/DB read, so these need no database:
    // every near-miss must keep the usage error instead of starting the
    // service (with or without an override).
    for args in [
        &["--allow-owwed"][..],
        &["--allow-owed-releases"][..],
        &["--allow-owed=1"][..],
        &["--allow_owed"][..],
        &["--frobnicate"][..],
        &["frobnicate"][..],
    ] {
        let output = cli(args, &[]).await;
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?} must stay a usage error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !text.contains("moderation_disable_override"),
            "{args:?} must never activate the override: {text}"
        );
        assert!(
            !text.contains("moderation_disable_refused"),
            "{args:?} must never start the service: {text}"
        );
    }
}

#[tokio::test]
async fn exact_flag_beside_an_unknown_argument_still_exits_2() {
    // Recognizing the one documented flag must not become accepting
    // arbitrary extra arguments on the service-start command line.
    for args in [
        &["--allow-owed", "--frobnicate"][..],
        &["--allow-owed", "frobnicate"][..],
    ] {
        let output = cli(args, &[]).await;
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?} must stay a usage error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !text.contains("moderation_disable_override"),
            "{args:?} must never activate the override: {text}"
        );
    }
}

#[tokio::test]
async fn operator_subcommands_do_not_inherit_the_service_start_flag() {
    // The flag appended to an operator command is that command's own
    // unexpected argument, never a silent service-start override.
    for args in [&["backup", "--allow-owed"][..], &["db", "--allow-owed"][..]] {
        let output = cli(args, &[]).await;
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?} must stay a usage error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !text.contains("moderation_disable_override"),
            "{args:?} must never activate the override: {text}"
        );
    }
}

#[tokio::test]
async fn moderation_preflight_flag_and_refusal_semantics_unchanged() {
    let Some(db) = migrated().await else { return };
    seed_owed(db.pool()).await;
    let url = db_url(&db);

    // The operator check's own flag keeps working exactly as before.
    let overridden = cli(
        &["moderation", "preflight", "--allow-owed"],
        &[("TWO_DATABASE_URL", &url)],
    )
    .await;
    assert_eq!(overridden.status.code(), Some(0));
    let text = String::from_utf8_lossy(&overridden.stdout);
    assert!(text.contains("OVERRIDE"), "{text}");
    assert!(text.contains("req-flag-owed"), "{text}");

    // Without either override the same check still refuses, naming ids.
    let refused = cli(&["moderation", "preflight"], &[("TWO_DATABASE_URL", &url)]).await;
    assert_eq!(refused.status.code(), Some(1));
    let text = String::from_utf8_lossy(&refused.stdout);
    assert!(text.contains("REFUSED"), "{text}");
    assert!(text.contains("req-flag-owed"), "{text}");
    db.close().await.expect("drops fixture database");
}
