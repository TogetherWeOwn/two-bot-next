//! Scheduled-message store integration tests (TOG-10081).
//!
//! Exercises `two-bot-core`'s `scheduled_store` (sqlx) against a per-card
//! scratch database on agent-testdb (`two_bot_test_tog10081`) — never staging
//! or production, per the card's test-DB rule. Each test owns one guild id so
//! parallel tests never share rows; every test deletes its guild's leftovers
//! first, so re-runs converge.
//!
//! Skips gracefully when agent-testdb is unreachable (CI has none), so
//! `cargo test --workspace` stays green everywhere: a failed setup prints
//! `SKIP …` and returns instead of asserting.

use std::time::Duration;

use two_bot_core::{
    audit_scheduled, claim_due, complete_run, delete_scheduled, format_iso_ms, get_scheduled,
    list_scheduled, put_scheduled, resolve_scheduled_id_store, retry_scheduled,
    ScheduledAuditInput, ScheduledWrite,
};
use two_bot_cutover::CutoverDb;

const ADMIN_URL: &str = "postgres://agent_test@agent-testdb:5432/postgres";
const TEST_DB: &str = "two_bot_test_tog10081";

/// Fixed clock: 2026-01-01T00:00:00.000Z, so every timestamp is exact.
const T0: u64 = 1_767_225_600_000;
const NOW: u64 = T0 + 600_000; // 00:10, the tick time
const LEASE: u64 = NOW + 60_000; // claim parks the row here

fn test_db_url() -> String {
    std::env::var("TWO_TEST_DATABASE_URL")
        .unwrap_or_else(|_| format!("postgres://agent_test@agent-testdb:5432/{TEST_DB}"))
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
    let _ = sqlx::query("DELETE FROM scheduled_messages WHERE guild_id = $1")
        .bind(guild)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM automation_audit_log WHERE guild_id = $1")
        .bind(guild)
        .execute(pool)
        .await;
}

async fn setup() -> Option<CutoverDb> {
    let inner = async {
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(3))
            .connect(ADMIN_URL)
            .await
            .ok()?;
        // Already-exists is the normal re-run path; any outcome is fine.
        // Mirrors TEST_DB in a literal: sqlx 0.9 only accepts `&'static str`
        // here (SqlSafeStr) and the name is a fixed alphanumeric const.
        let _ = sqlx::query("CREATE DATABASE \"two_bot_test_tog10081\"")
            .execute(&admin)
            .await;
        admin.close().await;
        two_bot_cutover::connect(&test_db_url(), 5, false)
            .await
            .ok()
    };
    match tokio::time::timeout(Duration::from_secs(20), inner).await {
        Ok(db) => db,
        Err(_) => {
            eprintln!("SKIP scheduled_store: agent-testdb unreachable (CI has none)");
            None
        }
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

    let claimed = claim_due(
        pool,
        guild,
        &iso(NOW),
        "claim-1",
        &iso(LEASE),
        10,
        "nonce-1",
    )
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
    let reclaimed = claim_due(
        pool,
        guild,
        &iso(NOW),
        "claim-2",
        &iso(LEASE),
        10,
        "nonce-2",
    )
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
        10,
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
            10,
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
    let claimed = claim_due(
        pool,
        guild,
        &iso(NOW),
        "claim-h",
        &iso(LEASE),
        10,
        "nonce-h",
    )
    .await
    .unwrap();
    // "edited" (next_run_at T0+5s) is also due; find the recurring row.
    assert!(claimed.iter().any(|r| r.id == "hourly"));
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
    let claimed = claim_due(
        pool,
        guild,
        &iso(NOW),
        "claim-f",
        &iso(LEASE),
        10,
        "nonce-f",
    )
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
        10,
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

    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM automation_audit_log WHERE guild_id = $1")
            .bind(guild)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}
