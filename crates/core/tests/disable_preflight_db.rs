//! Disable-guard acceptance against the disposable agent-testdb service.
//!
//! Ports legacy two-bot `test/e2e.moderation-disable-preflight.test.ts`
//! (refuse while unbans/lockdowns are owed, name the ids, clear control) and
//! `test/e2e.moderation-shutdown.test.ts` (boot with the flag off refuses;
//! the override proceeds and reports). Never touches staging or production:
//! every case mints its own migrated database through `TestDatabase` and
//! drops it on close.

#![cfg(feature = "db")]

use std::collections::HashMap;

use two_bot_core::disable_preflight::{
    boot_check, outstanding, BootVerdict, DisableGates, OwedReleases, STRANDED_RUNNING_DOC,
};
use two_bot_testsupport::TestDatabase;

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn gates_off() -> DisableGates {
    DisableGates {
        moderation: false,
        automations: false,
    }
}

async fn fixture() -> Option<TestDatabase> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(
            TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
                .await
                .expect("create migrated test database"),
        ),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure disable-preflight acceptance"
            );
            eprintln!("SKIP disable_preflight_db: TWO_TEST_DATABASE_URL not set");
            None
        }
    }
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

async fn seed_lockdown(pool: &sqlx::PgPool, channel: &str) {
    sqlx::query(
        "INSERT INTO moderation_lockdowns
           (channel_id, guild_id, prior_allow, prior_deny, prior_exists, reason, locked_at)
         VALUES ($1, 'g1', '0', '0', FALSE, 'raid', '2026-01-01T00:00:00Z')",
    )
    .bind(channel)
    .execute(pool)
    .await
    .expect("seeds a lockdown");
}

async fn seed_scheduled(pool: &sqlx::PgPool, id: &str, enabled: bool) {
    sqlx::query(
        "INSERT INTO scheduled_messages
           (id, guild_id, channel_id, body, next_run_at, enabled,
            created_by, created_at, updated_by, updated_at)
         VALUES ($1, 'g1', 'c1', 'hello', '2026-02-01T00:00:00.000Z', $2,
                 'op', '2026-01-01T00:00:00.000Z', 'op', '2026-01-01T00:00:00.000Z')",
    )
    .bind(id)
    .bind(enabled)
    .execute(pool)
    .await
    .expect("seeds a scheduled message");
}

#[tokio::test]
async fn clear_on_empty_database() {
    let Some(db) = fixture().await else { return };
    let owed = outstanding(db.pool()).await.expect("reads empty state");
    assert_eq!(owed, OwedReleases::default());
    assert_eq!(
        boot_check(db.pool(), &gates_off(), false)
            .await
            .expect("checks empty state"),
        BootVerdict::Proceed,
    );
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn non_terminal_unban_states_refuse_naming_request_ids() {
    let Some(db) = fixture().await else { return };
    for (request, state) in [
        ("req-staged", "staged"),
        ("req-pending", "pending"),
        ("req-running", "running"),
        ("req-quarantined", "quarantined"),
    ] {
        seed_unban(db.pool(), request, state).await;
    }
    let owed = outstanding(db.pool()).await.expect("reads owed unbans");
    assert_eq!(
        owed.unbans,
        vec![
            "req-pending",
            "req-quarantined",
            "req-running",
            "req-staged",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>(),
    );
    assert_eq!(owed.running_unbans, vec!["req-running".to_owned()]);
    let report = owed.report();
    for id in [
        "req-staged",
        "req-pending",
        "req-running",
        "req-quarantined",
    ] {
        assert!(report.contains(id), "refusal names {id}: {report}");
    }
    assert_eq!(
        boot_check(db.pool(), &gates_off(), false)
            .await
            .expect("checks owed unbans"),
        BootVerdict::Refused(owed),
    );
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn imported_enum_unban_states_keep_ids_and_running_tags() {
    let Some(db) = fixture().await else { return };
    for (request, state) in [
        ("req-staged", "staged"),
        ("req-pending", "pending"),
        ("req-running", "running"),
        ("req-quarantined", "quarantined"),
        ("req-done", "done"),
        ("req-superseded", "superseded"),
        ("req-cancelled", "cancelled"),
    ] {
        seed_unban(db.pool(), request, state).await;
    }
    // Imported deployments may retain an enum state column (migration 0111).
    // Alter only this disposable fixture, preserving rows and the pending index.
    for statement in [
        "CREATE TYPE imported_unban_state AS ENUM
           ('staged', 'pending', 'running', 'quarantined', 'done', 'superseded', 'cancelled')",
        "DROP INDEX uq_moderation_pending_unban",
        "ALTER TABLE moderation_scheduled_unbans ALTER COLUMN state
           TYPE imported_unban_state USING state::imported_unban_state",
        "CREATE UNIQUE INDEX uq_moderation_pending_unban
           ON moderation_scheduled_unbans (guild_id, user_id) WHERE state = 'pending'",
    ] {
        sqlx::query(statement)
            .execute(db.pool())
            .await
            .expect("creates the imported enum fixture");
    }
    let owed = outstanding(db.pool()).await;
    let verdict = boot_check(db.pool(), &gates_off(), false).await;
    db.close().await.expect("drops fixture database");

    let owed = owed.expect("reads imported enum states rather than UNKNOWN");
    assert_eq!(
        owed.unbans,
        vec![
            "req-pending",
            "req-quarantined",
            "req-running",
            "req-staged"
        ],
    );
    assert_eq!(owed.running_unbans, vec!["req-running"]);
    let report = owed.report();
    assert!(
        report.contains("1 running unban claim(s) [running]: req-running"),
        "{report}"
    );
    assert!(report.contains(STRANDED_RUNNING_DOC), "{report}");
    assert_eq!(
        verdict.expect("enum-backed boot guard names owed releases"),
        BootVerdict::Refused(owed),
    );
}

#[tokio::test]
async fn running_unban_left_by_a_stopped_worker_is_tagged_with_the_recovery_pointer() {
    let Some(db) = fixture().await else { return };
    // A worker claimed the job, dispatched the DELETE and stopped before it
    // recorded the outcome: the row keeps `running` and its claim token.
    seed_unban(db.pool(), "req-stranded", "running").await;
    sqlx::query(
        "UPDATE moderation_scheduled_unbans
            SET claim_token = 'secret-claim-token', claimed_at = '2026-02-01T00:00:00Z'
          WHERE request_id = 'req-stranded'",
    )
    .execute(db.pool())
    .await
    .expect("marks the claim as held by a stopped worker");
    seed_unban(db.pool(), "req-waiting", "pending").await;

    let owed = outstanding(db.pool()).await.expect("reads owed unbans");
    assert_eq!(
        owed.unbans,
        vec!["req-stranded".to_owned(), "req-waiting".to_owned()],
        "running stays in the existing owed-unban list"
    );
    assert_eq!(owed.running_unbans, vec!["req-stranded".to_owned()]);
    let report = owed.report();
    assert!(
        report.contains("1 pending unban(s): req-waiting"),
        "{report}"
    );
    assert!(
        report.contains("1 running unban claim(s) [running]: req-stranded"),
        "{report}"
    );
    assert!(report.contains("never drains on its own"), "{report}");
    assert!(report.contains(STRANDED_RUNNING_DOC), "{report}");
    assert!(
        !report.contains("secret-claim-token"),
        "the claim token fences the close and is never reported: {report}"
    );
    // The boot guard refuses and carries the same detail.
    assert_eq!(
        boot_check(db.pool(), &gates_off(), false)
            .await
            .expect("checks the stranded claim"),
        BootVerdict::Refused(owed),
    );
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn pending_only_unbans_refuse_without_running_tag_or_recovery_steps() {
    let Some(db) = fixture().await else { return };
    for (request, state) in [
        ("req-staged", "staged"),
        ("req-pending", "pending"),
        ("req-quarantined", "quarantined"),
    ] {
        seed_unban(db.pool(), request, state).await;
    }
    let owed = outstanding(db.pool()).await.expect("reads owed unbans");
    assert!(owed.running_unbans.is_empty());
    assert_eq!(owed.unbans.len(), 3);
    let report = owed.report();
    assert!(
        report.starts_with("REFUSED: 3 pending unban(s)"),
        "{report}"
    );
    for absent in ["running", "[running]", STRANDED_RUNNING_DOC] {
        assert!(!report.contains(absent), "{absent:?} in {report}");
    }
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn terminal_unban_states_owe_nothing() {
    let Some(db) = fixture().await else { return };
    for (request, state) in [
        ("req-done", "done"),
        ("req-superseded", "superseded"),
        ("req-cancelled", "cancelled"),
    ] {
        seed_unban(db.pool(), request, state).await;
    }
    let owed = outstanding(db.pool()).await.expect("reads terminal unbans");
    assert!(owed.unbans.is_empty());
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn active_lockdown_refuses_naming_channel() {
    let Some(db) = fixture().await else { return };
    seed_lockdown(db.pool(), "chan-owed-1").await;
    let owed = outstanding(db.pool()).await.expect("reads owed lockdown");
    assert_eq!(owed.lockdowns, vec!["chan-owed-1".to_owned()]);
    assert!(
        owed.report().contains("chan-owed-1"),
        "refusal names the channel: {}",
        owed.report()
    );
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn enabled_scheduled_refuses_but_disabled_is_clear() {
    let Some(db) = fixture().await else { return };
    seed_scheduled(db.pool(), "msg-live", true).await;
    seed_scheduled(db.pool(), "msg-paused", false).await;
    let owed = outstanding(db.pool()).await.expect("reads scheduled state");
    assert_eq!(owed.scheduled, vec!["msg-live".to_owned()]);
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn missing_tables_read_as_empty_never_unknown() {
    let Some(db) = fixture().await else { return };
    // A database that never ran a slice's migrations cannot hold its releases.
    sqlx::query("DROP TABLE scheduled_messages")
        .execute(db.pool())
        .await
        .expect("drops the late slice table");
    sqlx::query("DROP TABLE moderation_lockdowns")
        .execute(db.pool())
        .await
        .expect("drops the channel slice table");
    sqlx::query("DROP TABLE moderation_scheduled_unbans")
        .execute(db.pool())
        .await
        .expect("drops the member slice table");
    let owed = outstanding(db.pool())
        .await
        .expect("missing tables are empty, not an error");
    assert!(owed.is_clear());
    assert_eq!(
        boot_check(db.pool(), &gates_off(), false)
            .await
            .expect("missing tables proceed"),
        BootVerdict::Proceed,
    );
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn boot_check_honors_gates_and_override() {
    let Some(db) = fixture().await else { return };
    seed_unban(db.pool(), "req-owed", "pending").await;
    seed_scheduled(db.pool(), "msg-owed", true).await;

    // Moderation off alone still refuses the owed unban (scheduled ignored).
    let moderation_only = DisableGates {
        moderation: false,
        automations: true,
    };
    let verdict = boot_check(db.pool(), &moderation_only, false)
        .await
        .expect("checks moderation slice");
    assert_eq!(
        verdict,
        BootVerdict::Refused(OwedReleases {
            unbans: vec!["req-owed".to_owned()],
            ..OwedReleases::default()
        }),
    );

    // Automations off alone still refuses the owed schedule (unbans ignored).
    let automations_only = DisableGates {
        moderation: true,
        automations: false,
    };
    let verdict = boot_check(db.pool(), &automations_only, false)
        .await
        .expect("checks automations slice");
    assert_eq!(
        verdict,
        BootVerdict::Refused(OwedReleases {
            scheduled: vec!["msg-owed".to_owned()],
            ..OwedReleases::default()
        }),
    );

    // The explicit override proceeds but carries the owed set for the log.
    let verdict = boot_check(db.pool(), &gates_off(), true)
        .await
        .expect("override proceeds");
    assert!(matches!(verdict, BootVerdict::Overridden(_)));
    assert!(!matches!(verdict, BootVerdict::Proceed));

    // Both gates on short-circuits before any database read: even a closed
    // pool proceeds, so enabled boot pays no query and cannot fail here.
    let closed = db.pool().clone();
    closed.close().await;
    assert_eq!(
        boot_check(
            &closed,
            &DisableGates {
                moderation: true,
                automations: true,
            },
            false,
        )
        .await
        .expect("enabled gates skip the database"),
        BootVerdict::Proceed,
    );
    db.close().await.expect("drops fixture database");
}

#[test]
fn gates_read_exact_one_from_map() {
    assert_eq!(
        DisableGates::from_map(&vars(&[("TWO_MODERATION", "1")])),
        DisableGates {
            moderation: true,
            automations: false,
        }
    );
}
