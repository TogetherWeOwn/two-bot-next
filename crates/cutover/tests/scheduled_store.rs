//! Scheduled-message store integration tests (TOG-10081).
//!
//! Exercises `two-bot-core`'s `scheduled_store` (sqlx) against
//! `TWO_TEST_DATABASE_URL`: a pre-provisioned scratch database on agent-testdb
//! or the CI Postgres service container, never staging or production. Each
//! test owns one guild id so parallel tests never share rows; every test
//! deletes its guild's leftovers first, so re-runs converge.
//!
//! Local runs without an explicit URL skip DB tests. CI requires the URL.
//! Once configured, all URL, connection, migration and cleanup errors fail
//! the suite — an unavailable database is never reported as a passing skip.

use std::time::Duration;

use two_bot_core::{
    audit_scheduled, claim_due, complete_run, delete_scheduled, format_iso_ms, get_scheduled,
    list_scheduled, put_scheduled, resolve_scheduled_id_store, retry_scheduled,
    ScheduledAuditInput, ScheduledWrite,
};
use two_bot_cutover::CutoverDb;

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

async fn scrub(pool: &sqlx::PgPool, guild: &str) {
    sqlx::query("DELETE FROM scheduled_messages WHERE guild_id = $1")
        .bind(guild)
        .execute(pool)
        .await
        .expect("scheduled-message cleanup failed");
    sqlx::query("DELETE FROM automation_audit_log WHERE guild_id = $1")
        .bind(guild)
        .execute(pool)
        .await
        .expect("automation-audit cleanup failed");
}

async fn setup() -> Option<CutoverDb> {
    let ci = std::env::var("CI").is_ok_and(|value| value == "true");
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) if !ci => return None,
        Err(_) => panic!("TWO_TEST_DATABASE_URL must be configured for CI"),
    };
    validate_test_db_url(&url, ci).expect("unsafe or invalid test database configuration");
    let db = tokio::time::timeout(
        Duration::from_secs(20),
        two_bot_cutover::connect(&url, 5, false),
    )
    .await
    .expect("test database initialization timed out")
    .expect("test database connection or migration failed");
    Some(db)
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
    let Some(db) = setup().await else {
        eprintln!("SKIP legacy_queue_upgrade_preserves_rows_and_decodes_store_shape: no test db");
        return;
    };
    for has_claim_columns in [false, true] {
        // Transactional schema isolation: never replace the shared test tables.
        let mut tx = db.pool().begin().await.unwrap();
        sqlx::raw_sql(
            "CREATE SCHEMA scheduled_legacy_upgrade;
             SET LOCAL search_path = scheduled_legacy_upgrade, pg_catalog;",
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
            "interval_seconds = 59",
            "interval_seconds = 31536001",
            "body = ''",
        ] {
            sqlx::query("SAVEPOINT invalid_legacy_write")
                .execute(&mut *tx)
                .await
                .unwrap();
            let error = sqlx::query(&format!(
                "UPDATE scheduled_messages SET {invalid} WHERE id = 'legacy-hourly'"
            ))
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
}

#[tokio::test]
async fn put_get_list_round_trip_in_ticker_order() {
    let Some(db) = setup().await else {
        eprintln!("SKIP put_get_list_round_trip_in_ticker_order: no test db");
        return;
    };
    let pool = db.pool();
    let guild = "sched-list";
    scrub(pool, guild).await;

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
}

#[tokio::test]
async fn prefixes_treat_like_metacharacters_literally() {
    let Some(db) = setup().await else {
        eprintln!("SKIP prefixes_treat_like_metacharacters_literally: no test db");
        return;
    };
    let pool = db.pool();
    let guild = "sched-literal-prefix";
    scrub(pool, guild).await;
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
}

#[tokio::test]
async fn two_due_messages_claim_independently_and_retry_keeps_nonce() {
    let Some(db) = setup().await else {
        eprintln!("SKIP two_due_messages_claim_independently_and_retry_keeps_nonce: no test db");
        return;
    };
    let pool = db.pool();
    let guild = "sched-two-due";
    scrub(pool, guild).await;
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
}

#[tokio::test]
async fn claim_happens_once_then_one_shot_disables() {
    let Some(db) = setup().await else {
        eprintln!("SKIP claim_happens_once_then_one_shot_disables: no test db");
        return;
    };
    let pool = db.pool();
    let guild = "sched-claim-once";
    scrub(pool, guild).await;

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
}

#[tokio::test]
async fn stale_completion_loses_and_recurring_advances() {
    let Some(db) = setup().await else {
        eprintln!("SKIP stale_completion_loses_and_recurring_advances: no test db");
        return;
    };
    let pool = db.pool();
    let guild = "sched-stale-recur";
    scrub(pool, guild).await;

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
}

#[tokio::test]
async fn retry_requeues_keeping_nonce() {
    let Some(db) = setup().await else {
        eprintln!("SKIP retry_requeues_keeping_nonce: no test db");
        return;
    };
    let pool = db.pool();
    let guild = "sched-retry";
    scrub(pool, guild).await;

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
}

#[tokio::test]
async fn schedule_ids_are_guild_scoped() {
    let Some(db) = setup().await else {
        eprintln!("SKIP schedule_ids_are_guild_scoped: no test db");
        return;
    };
    let pool = db.pool();
    let (a, b) = ("sched-scope-a", "sched-scope-b");
    scrub(pool, a).await;
    scrub(pool, b).await;

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
}

#[tokio::test]
async fn audit_rows_record_outcomes() {
    let Some(db) = setup().await else {
        eprintln!("SKIP audit_rows_record_outcomes: no test db");
        return;
    };
    let pool = db.pool();
    let guild = "sched-audit";
    scrub(pool, guild).await;

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
}
