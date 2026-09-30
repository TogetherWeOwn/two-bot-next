#![cfg(feature = "db")]
//! Live sticky store verification (TOG-10082) — agent-testdb only.
//!
//! Ignored by default: needs a reachable Postgres. Run with:
//! `cargo test -p two-bot-core --features db -- --ignored sticky_live`
//!
//! The default URL targets the documented agent-testdb credential
//! (`agent_test`, empty password); override with
//! `STICKY_TEST_DATABASE_URL`. The harness refuses any host that is not a
//! test host, so this can never run against staging or production by
//! accident. One scratch schema per run (`sticky_tog10082_<pid>`) carries
//! the tables; it is dropped at the end.
//!
//! Applies the real `0150_sticky_messages.sql` migration (single-statement
//! `raw_sql`, so the checked-in file is what gets verified), then drives
//! the card acceptance end to end: bursts coalesce to one re-post, remove
//! clears state, updates preserve the live post columns.

use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use two_bot_core::sticky::store::{
    audit_sticky, claim_sticky_post, delete_sticky, get_sticky, put_sticky, record_sticky_post,
    release_sticky_post,
};
use two_bot_core::sticky::{
    PutSticky, RemoveOutcome, StickyAudit, StickyAuditAction, StickyAuditOutcome,
};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as i64
}

fn test_url() -> String {
    std::env::var("STICKY_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://agent_test@agent-testdb:5432/agent_test".to_owned())
}

#[tokio::test]
#[ignore]
async fn sticky_live_round_trip() {
    let url = test_url();
    assert!(
        url.contains("testdb") || url.contains("localhost") || url.contains("127.0.0.1"),
        "refusing non-test host"
    );

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect agent-testdb (see crate docs for the expected credential)");
    let schema = format!("sticky_tog10082_{}", std::process::id());
    // Audited: `schema` is a fixed prefix plus the process id (digits only),
    // so no injection is possible.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .expect("create scratch schema");
    admin.close().await;

    let mut options: PgConnectOptions = url.parse().expect("parse test url");
    options = options.options([("search_path", schema.as_str())]);
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("connect scratch schema");

    // The checked-in migration is what gets applied.
    let ddl = include_str!("../../cutover/migrations/0150_sticky_messages.sql");
    sqlx::raw_sql(ddl)
        .execute(&pool)
        .await
        .expect("0150 migration applies cleanly");

    let now = now_ms();
    let (guild, held, burst, gone) = ("g-live", "c-held", "c-burst", "c-remove");

    // Held channel (debounce 300): set → post → immediate activity is held,
    // so a burst coalesces to no re-post.
    let created = put_sticky(
        &pool,
        &PutSticky {
            guild_id: guild,
            channel_id: held,
            body: "held sticky",
            debounce_seconds: 300,
            enabled: true,
            actor_id: "admin-1",
            now_ms: now,
        },
    )
    .await
    .expect("put sticky");
    assert!(created);
    let row = get_sticky(&pool, guild, held)
        .await
        .expect("get sticky")
        .expect("row exists");
    assert_eq!((row.debounce_seconds, row.last_posted_at_ms), (300, None));

    let grant = claim_sticky_post(&pool, guild, held, "claim-1", now)
        .await
        .expect("first claim")
        .expect("never-posted sticky is due");
    assert_eq!(grant.body, "held sticky");
    assert_eq!(grant.previous_message_id, None);
    assert!(
        record_sticky_post(&pool, guild, held, "m-1", now, "claim-1")
            .await
            .expect("record")
    );
    assert!(
        claim_sticky_post(&pool, guild, held, "claim-2", now + 1_000)
            .await
            .expect("second claim")
            .is_none(),
        "1s after a 300s-debounce post the burst is held"
    );

    // Burst channel (debounce 1): backdate the live post, then race two
    // claims — exactly one wins the re-post window.
    put_sticky(
        &pool,
        &PutSticky {
            guild_id: guild,
            channel_id: burst,
            body: "burst sticky",
            debounce_seconds: 1,
            enabled: true,
            actor_id: "admin-1",
            now_ms: now,
        },
    )
    .await
    .expect("put burst sticky");
    claim_sticky_post(&pool, guild, burst, "seed", now)
        .await
        .expect("seed claim")
        .expect("due");
    assert!(
        record_sticky_post(&pool, guild, burst, "m-old", now - 5_000, "seed")
            .await
            .expect("seed record")
    );
    // An update preserves the live post columns (legacy putSticky keeps
    // lastMessageId/lastPostedAt on conflict).
    let updated = put_sticky(
        &pool,
        &PutSticky {
            guild_id: guild,
            channel_id: burst,
            body: "burst sticky v2",
            debounce_seconds: 1,
            enabled: true,
            actor_id: "admin-2",
            now_ms: now,
        },
    )
    .await
    .expect("update burst sticky");
    assert!(!updated);
    let row = get_sticky(&pool, guild, burst)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(row.body, "burst sticky v2");
    assert_eq!(row.last_message_id.as_deref(), Some("m-old"));

    let (win_a, win_b) = tokio::join!(
        claim_sticky_post(&pool, guild, burst, "race-a", now),
        claim_sticky_post(&pool, guild, burst, "race-b", now),
    );
    let winners = [win_a.expect("race-a"), win_b.expect("race-b")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(winners.len(), 1, "bursts coalesce to one re-post");
    assert_eq!(winners[0].body, "burst sticky v2");
    assert_eq!(winners[0].previous_message_id.as_deref(), Some("m-old"));

    // Release path: a failed post frees the window for the next attempt.
    put_sticky(
        &pool,
        &PutSticky {
            guild_id: guild,
            channel_id: gone,
            body: "gone sticky",
            debounce_seconds: 1,
            enabled: true,
            actor_id: "admin-1",
            now_ms: now,
        },
    )
    .await
    .expect("put remove sticky");
    assert!(claim_sticky_post(&pool, guild, gone, "doomed", now)
        .await
        .expect("doomed claim")
        .is_some());
    release_sticky_post(&pool, guild, gone, "doomed")
        .await
        .expect("release");
    let grant = claim_sticky_post(&pool, guild, gone, "retry", now)
        .await
        .expect("retry claim")
        .expect("released window is claimable");
    assert_eq!(grant.previous_message_id, None);
    assert!(
        record_sticky_post(&pool, guild, gone, "m-gone", now, "retry")
            .await
            .expect("record")
    );

    // Remove clears state: first delete returns the live message id for
    // Discord cleanup, the second reports absent.
    assert_eq!(
        delete_sticky(&pool, guild, gone).await.expect("delete"),
        RemoveOutcome::Removed {
            previous_message_id: Some("m-gone".to_owned())
        }
    );
    assert!(get_sticky(&pool, guild, gone).await.expect("get").is_none());
    assert_eq!(
        delete_sticky(&pool, guild, gone).await.expect("re-delete"),
        RemoveOutcome::Absent
    );

    // Audit rows land for the sticky.* actions.
    for (action, outcome) in [
        (StickyAuditAction::Create, StickyAuditOutcome::Ok),
        (StickyAuditAction::Run, StickyAuditOutcome::Ok),
        (StickyAuditAction::Delete, StickyAuditOutcome::Absent),
    ] {
        audit_sticky(
            &pool,
            &StickyAudit {
                guild_id: guild,
                actor_id: Some("admin-1"),
                action,
                target_key: Some(held),
                outcome,
                reason: None,
                at_ms: now,
            },
        )
        .await
        .expect("audit");
    }
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM automation_audit_log")
        .fetch_one(&pool)
        .await
        .expect("count audits");
    assert_eq!(count, 3);

    pool.close().await;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("reconnect for cleanup");
    // Audited: same PID-derived name as the CREATE above.
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .expect("drop scratch schema");
    admin.close().await;
}
