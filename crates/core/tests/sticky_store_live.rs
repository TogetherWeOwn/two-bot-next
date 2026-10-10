#![cfg(feature = "db")]
//! Live sticky store verification (TOG-10082) — agent-testdb only.
//!
//! Ignored by default: needs a reachable Postgres. Run with:
//! `cargo test -p two-bot-core --features db -- --ignored sticky_live`
//!
//! The default URL targets the documented agent-testdb credential
//! (`agent_test`, empty password); override with
//! `STICKY_TEST_DATABASE_URL`. Parsed options must target `agent-testdb:5432`,
//! user/database `agent_test`, empty password, no Unix socket. Loopback and
//! lookalike hosts are not approved test containers. Each live test creates
//! its own scratch schema and drops it at the end.
//!
//! Applies the real `0150_sticky_messages.sql` migration (single-statement
//! `raw_sql`, so the checked-in file is what gets verified), then drives
//! the card acceptance end to end: bursts coalesce to one re-post, remove
//! clears state, updates preserve the live post columns.

use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};
use two_bot_core::sticky::store::{
    audit_sticky, claim_sticky_post, delete_sticky, get_sticky, put_sticky, record_sticky_post,
    release_sticky_post,
};
use two_bot_core::sticky::{
    decide_activity, ActivityDecision, PutSticky, RemoveOutcome, StickyAudit, StickyAuditAction,
    StickyAuditOutcome,
};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as i64
}

const TEST_URL: &str = "postgres://agent_test:@agent-testdb:5432/agent_test";
const DDL: &str = include_str!("../../cutover/migrations/0150_sticky_messages.sql");

fn test_options(url: &str) -> Result<PgConnectOptions, &'static str> {
    // sqlx applies query-string overrides (host, hostaddr, user, dbname,
    // port) while parsing. Validate the effective options, not URL substrings.
    let options: PgConnectOptions = url.parse().map_err(|_| "invalid test URL")?;
    if options.get_host() != "agent-testdb" || options.get_socket().is_some() {
        return Err("refusing non-test host");
    }
    if options.get_port() != 5432
        || options.get_username() != "agent_test"
        || options.get_database() != Some("agent_test")
        || options
            .to_url_lossy()
            .password()
            .is_some_and(|password| !password.is_empty())
    {
        return Err("refusing non-test database or credential");
    }
    // Never fall back to an inherited PGPASSWORD or .pgpass credential.
    Ok(options.password(""))
}

#[test]
fn sticky_test_options_require_the_approved_container() {
    assert!(test_options(TEST_URL).is_ok());
    for url in [
        "postgres://agent_test@production.example/testdb",
        "postgres://agent_test:localhost@production.example/realdb",
        "postgres://agent_test:@agent-testdb.production.example/agent_test",
        "postgres://agent_test:@localhost/agent_test",
        "postgres://agent_test:@127.0.0.1/agent_test",
        "postgres://agent_test:@agent-testdb/agent_test?host=production.example",
        "postgres://agent_test:@agent-testdb/agent_test?hostaddr=192.0.2.1",
        "postgres://agent_test:@agent-testdb/agent_test?host=/var/run/postgresql",
        "postgres://other_user:@agent-testdb/agent_test",
        "postgres://agent_test:@agent-testdb/production",
        "postgres://agent_test:@agent-testdb/agent_test?user=other_user",
        "postgres://agent_test:@agent-testdb/agent_test?dbname=production",
        "postgres://agent_test:@agent-testdb/agent_test?port=5433",
        "postgres://agent_test:fixture-password@agent-testdb/agent_test",
    ] {
        assert!(
            test_options(url).is_err(),
            "unsafe test options were accepted"
        );
    }
}

async fn scratch_pool(name: &str) -> (PgPool, PgConnectOptions, String) {
    let url = std::env::var("STICKY_TEST_DATABASE_URL").unwrap_or_else(|_| TEST_URL.to_owned());
    let options = test_options(&url).expect("only documented agent-testdb options are allowed");
    assert!(name.bytes().all(|byte| byte.is_ascii_lowercase()));
    let schema = format!(
        "sticky_tog10082_{}_{}_{}",
        std::process::id(),
        now_ms(),
        name
    );
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .expect("connect agent-testdb (see crate docs for the expected credential)");
    // Audited: schema consists only of a fixed prefix, digits and the
    // validated lowercase test name; no caller-supplied URL enters SQL.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .expect("create scratch schema");
    admin.close().await;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options.clone().options([("search_path", schema.as_str())]))
        .await
        .expect("connect scratch schema");
    (pool, options, schema)
}

async fn cleanup(pool: PgPool, options: PgConnectOptions, schema: String) {
    pool.close().await;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("reconnect for cleanup");
    // Audited: same validated name as the CREATE above.
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .expect("drop scratch schema");
    admin.close().await;
}

#[tokio::test]
#[ignore = "requires reachable agent-testdb (STICKY_TEST_DATABASE_URL override); CI runs --ignored in ignored-db-stores"]
async fn sticky_live_round_trip() {
    let (pool, options, schema) = scratch_pool("fresh").await;

    // The checked-in migration is what gets applied.
    sqlx::raw_sql(DDL)
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

    cleanup(pool, options, schema).await;
}

#[tokio::test]
#[ignore = "requires reachable agent-testdb (STICKY_TEST_DATABASE_URL override); CI runs --ignored in ignored-db-stores"]
async fn sticky_live_legacy_upgrade() {
    let (pool, options, schema) = scratch_pool("legacy").await;
    sqlx::raw_sql(include_str!("fixtures/legacy_sticky.sql"))
        .execute(&pool)
        .await
        .expect("seed populated legacy TEXT timestamp schema");
    // Also prove reapplying the DDL to already-converted columns is safe.
    for _ in 0..2 {
        sqlx::raw_sql(DDL)
            .execute(&pool)
            .await
            .expect("upgrade legacy schema");
    }
    let (typed,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM information_schema.columns
         WHERE table_schema = $1 AND data_type = 'timestamp with time zone'
           AND ((table_name = 'sticky_messages'
                 AND column_name IN ('last_posted_at', 'created_at', 'updated_at', 'claimed_at'))
                OR (table_name = 'automation_audit_log' AND column_name = 'created_at'))",
    )
    .bind(&schema)
    .fetch_one(&pool)
    .await
    .expect("check all five upgraded timestamp types");
    if typed != 5 {
        cleanup(pool, options, schema).await;
        panic!("expected five converted timestamp columns, found {typed}");
    }

    const LEGACY_MS: i64 = 1_800_000_000_123;
    let (guild, posted, never) = ("g-legacy", "c-posted", "c-never");
    let row = get_sticky(&pool, guild, posted)
        .await
        .expect("read converted last_posted_at")
        .expect("legacy row retained");
    assert_eq!(row.body, "legacy sticky");
    assert_eq!(row.last_message_id.as_deref(), Some("m-legacy"));
    assert_eq!(row.last_posted_at_ms, Some(LEGACY_MS));
    // The pure precheck sees elapsed debounce, not the active claim. Its
    // Repost result does NOT authorize posting; the atomic claim below holds.
    assert!(matches!(
        decide_activity(Some(&row), false, true, true, LEGACY_MS + 5_000),
        ActivityDecision::Repost { .. }
    ));
    assert!(
        claim_sticky_post(&pool, guild, posted, "held", LEGACY_MS + 5_000)
            .await
            .expect("compare converted claimed_at")
            .is_none(),
        "active legacy claim remains fenced"
    );
    let grant = claim_sticky_post(&pool, guild, posted, "new-claim", LEGACY_MS + 60_000)
        .await
        .expect("claim at legacy expiry boundary")
        .expect("legacy claim expires at 60s");
    assert_eq!(grant.previous_message_id.as_deref(), Some("m-legacy"));
    assert!(record_sticky_post(
        &pool,
        guild,
        posted,
        "m-new",
        LEGACY_MS + 60_000,
        "new-claim"
    )
    .await
    .expect("record replacement on upgraded row"));
    assert!(!put_sticky(
        &pool,
        &PutSticky {
            guild_id: guild,
            channel_id: posted,
            body: "updated legacy sticky",
            debounce_seconds: 5,
            enabled: true,
            actor_id: "admin-new",
            now_ms: LEGACY_MS + 61_000,
        }
    )
    .await
    .expect("upsert timestamps on upgraded row"));
    let (created, updated): (i64, i64) = sqlx::query_as(
        "SELECT (extract(epoch from created_at) * 1000)::BIGINT,
                (extract(epoch from updated_at) * 1000)::BIGINT
         FROM sticky_messages WHERE guild_id = $1 AND channel_id = $2",
    )
    .bind(guild)
    .bind(posted)
    .fetch_one(&pool)
    .await
    .expect("creation timestamp preserved and update advanced");
    assert_eq!((created, updated), (LEGACY_MS, LEGACY_MS + 61_000));

    let row = get_sticky(&pool, guild, never)
        .await
        .expect("get never-posted")
        .expect("row");
    assert_eq!(row.last_posted_at_ms, None);
    assert!(claim_sticky_post(&pool, guild, never, "first", LEGACY_MS)
        .await
        .expect("NULL legacy timestamps survive conversion")
        .is_some());
    assert_eq!(
        delete_sticky(&pool, guild, never)
            .await
            .expect("remove legacy sticky"),
        RemoveOutcome::Removed {
            previous_message_id: None
        }
    );
    assert!(get_sticky(&pool, guild, never)
        .await
        .expect("removed")
        .is_none());

    let (audit_at,): (i64,) = sqlx::query_as(
        "SELECT (extract(epoch from created_at) * 1000)::BIGINT
         FROM automation_audit_log WHERE id = 'a-legacy'",
    )
    .fetch_one(&pool)
    .await
    .expect("read upgraded offset-bearing audit timestamp");
    assert_eq!(audit_at, LEGACY_MS);
    audit_sticky(
        &pool,
        &StickyAudit {
            guild_id: guild,
            actor_id: None,
            action: StickyAuditAction::Run,
            target_key: Some(posted),
            outcome: StickyAuditOutcome::Ok,
            reason: None,
            at_ms: LEGACY_MS + 60_000,
        },
    )
    .await
    .expect("append audit on upgraded schema");
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM automation_audit_log")
        .fetch_one(&pool)
        .await
        .expect("old and new audits coexist");
    assert_eq!(count, 2);
    cleanup(pool, options, schema).await;
}
