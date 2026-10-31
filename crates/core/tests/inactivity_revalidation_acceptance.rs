//! Protected acceptance suite: the inactivity sweep must revalidate member
//! eligibility atomically with recording `member_inactive`, not act on the
//! stale initial `SELECT`.
//!
//! The sweep first selects quiet members and only later, per member, opens a
//! transaction and takes `LOCK TABLE events, members, privacy_opt_outs IN
//! ROW EXCLUSIVE MODE`. A member can become active again, leave, become a
//! bot, opt out, or be flagged by another sweep inside that window. This
//! suite pins the outcome contract for that window:
//!
//! - a candidate that is no longer eligible at commit time produces no new
//!   `member_inactive` event, no `inactive_flagged_at` mutation, and no
//!   flagged outcome entry;
//! - a genuinely eligible candidate keeps the current event/outcome
//!   contract, and privacy suppression keeps working;
//! - two concurrent sweeps cannot create duplicate inactivity events for
//!   one quiet spell merely because their sweep timestamps (and therefore
//!   idempotency keys) differ.
//!
//! Determinism without any production test seam: a fixture transaction
//! holds `LOCK TABLE events IN SHARE MODE`. The sweep's initial `SELECT`
//! reads only `members`, so selection completes; its per-member `LOCK
//! TABLE ... IN ROW EXCLUSIVE MODE` then blocks on `events`. The test
//! waits until `pg_locks` shows that ungranted lock — proof the sweep has
//! selected and is paused before its first write — then mutates the member
//! row inside the fixture transaction and commits, which publishes the
//! mutation and releases the sweep in one atomic step. No elapsed-time
//! guesses: every wait is on an observed lock state, matching the
//! wait-for-lock pattern already used by other store suites.
//!
//! Disposable database only: `TWO_TEST_DATABASE_URL` absent skips (the
//! existing store-test convention); a present URL must name the approved
//! test container or the ephemeral CI Postgres service. No Discord, no
//! staging or production database, no credentials beyond the test role.
#![cfg(feature = "db")]

use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use two_bot_core::inactivity_store::run_sweep;

const GUILD: &str = "100000000000000090";
const STALE: &str = "100000000000000001";
const QUIET: &str = "100000000000000002";
const NOW: &str = "2026-09-07T00:00:00.000Z";
const NOW_PLUS_MINUTE: &str = "2026-09-07T00:01:00.000Z";
/// After the 14-day cutoff for `NOW` (2026-08-24): activity or a flag stamp
/// at this instant makes a member ineligible for that sweep.
const AFTER_CUTOFF: &str = "2026-09-06T00:00:00.000Z";
const JOINED: &str = "2026-07-01T00:00:00.000Z";
const DAYS: u64 = 14;

/// Only an absent URL skips the test. Configured setup failures are errors.
async fn test_pool(prefix: &str) -> Result<Option<(Pool<Postgres>, String)>, sqlx::Error> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(_) => panic!("TWO_TEST_DATABASE_URL must be Unicode"),
    };
    let options: PgConnectOptions = url.parse()?;
    // No inherited credentials: only the approved agent test container or
    // the ephemeral Postgres service in GitHub Actions (same guard as the
    // inactivity store unit tests).
    let host = options.get_host();
    assert!(
        host == "agent-testdb"
            || (host == "localhost" && std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")),
        "test-container-only URL"
    );
    assert_eq!(
        options.get_username(),
        "agent_test",
        "test-container-only user"
    );
    // Each test owns its schema, including concurrent invocations in other
    // processes, so the `events` relation the fixture locks is private.
    static NEXT_SCHEMA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let schema = format!(
        "{prefix}_{}_{nonce}_{}",
        std::process::id(),
        NEXT_SCHEMA.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    assert!(is_test_schema_name(&schema), "safe unique test schema");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await?;
    // Only the guarded identifier interpolates; values bind.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await?;
    admin.close().await;
    // Connections: the sweep's select and per-member transactions, the
    // fixture blocker transaction, and lock-state probes.
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await?;
    sqlx::raw_sql(include_str!("../../cutover/migrations/0001_funnel.sql"))
        .execute(&pool)
        .await?;
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS privacy_opt_outs (
           guild_id TEXT NOT NULL,
           member_id TEXT NOT NULL,
           opted_out_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
           PRIMARY KEY (guild_id, member_id)
         )",
    )
    .execute(&pool)
    .await?;
    Ok(Some((pool, schema)))
}

/// Test-only schema names: lowercase identifier characters, so DDL
/// interpolation cannot break out of the identifier position.
fn is_test_schema_name(schema: &str) -> bool {
    !schema.is_empty()
        && schema.len() <= 63
        && schema
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

async fn drop_schema(pool: &Pool<Postgres>, schema: &str) {
    assert!(is_test_schema_name(schema));
    // Guarded by `is_test_schema_name` (see `test_pool`).
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(pool)
        .await
        .expect("clean up owned test schema");
    pool.close().await;
}

/// A member quiet since joining: eligible for a 14-day sweep at `NOW`.
async fn seed_quiet_member(pool: &Pool<Postgres>, member: &str) {
    sqlx::query(
        "INSERT INTO members (guild_id, member_id, joined_at, last_active_at, is_bot, left_at)
         VALUES ($1, $2, $3::timestamptz, NULL, FALSE, NULL)",
    )
    .bind(GUILD)
    .bind(member)
    .bind(JOINED)
    .execute(pool)
    .await
    .expect("seed quiet member");
}

/// Wait until at least `expected` backends are blocked acquiring a lock on
/// this schema's `events` table — the observable proof that the sweep has
/// finished its initial select and is paused at its per-member table lock,
/// before any write. Bounded so a broken sweep fails instead of hanging.
async fn wait_for_sweeps_blocked_on_events_lock(pool: &Pool<Postgres>, expected: i64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_locks
                  WHERE relation = 'events'::regclass AND NOT granted",
            )
            .fetch_one(pool)
            .await
            .expect("lock state probe");
            if waiting >= expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("sweep must reach its per-member table lock after selecting");
}

/// What one interleaved sweep left behind, collected before schema cleanup
/// so assertions run after the schema is dropped even when they fail.
struct Observed {
    outcome_ids: Vec<String>,
    events_for_stale: i64,
    events_total: i64,
    stale_flagged_at: Option<String>,
}

async fn observe(pool: &Pool<Postgres>, outcome_ids: Vec<String>) -> Observed {
    let events_for_stale: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE event_type = 'member_inactive' AND member_id = $1",
    )
    .bind(STALE)
    .fetch_one(pool)
    .await
    .expect("member event count");
    let events_total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE event_type = 'member_inactive'")
            .fetch_one(pool)
            .await
            .expect("total event count");
    let stale_flagged_at: Option<String> = sqlx::query_scalar(
        "SELECT to_char(inactive_flagged_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
           FROM members WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(GUILD)
    .bind(STALE)
    .fetch_one(pool)
    .await
    .expect("flag timestamp");
    Observed {
        outcome_ids,
        events_for_stale,
        events_total,
        stale_flagged_at,
    }
}

/// Run one sweep paused between selection and its first write, apply
/// `interim_sql` (a single fixture statement against `STALE`, run inside
/// the blocking transaction), then release the sweep and collect what it
/// left behind.
///
/// The fixture mutation and the lock release are the same commit, so the
/// sweep can only ever observe the world strictly before or strictly after
/// the mutation — never a torn state, and never "mutation not yet applied".
async fn run_paused_sweep(pool: &Pool<Postgres>, interim_sql: Option<&str>) -> Vec<String> {
    let mut blocker = pool.begin().await.expect("fixture transaction");
    // SHARE conflicts with the sweep's ROW EXCLUSIVE table lock but not
    // with the ACCESS SHARE its initial member select needs.
    sqlx::query("LOCK TABLE events IN SHARE MODE")
        .execute(&mut *blocker)
        .await
        .expect("fixture holds the events gate");
    let sweep_pool = pool.clone();
    let sweep = tokio::spawn(async move { run_sweep(&sweep_pool, NOW, DAYS).await });
    wait_for_sweeps_blocked_on_events_lock(pool, 1).await;
    if let Some(sql) = interim_sql {
        sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
            .execute(&mut *blocker)
            .await
            .expect("interim fixture mutation");
    }
    blocker
        .commit()
        .await
        .expect("publish mutation, release sweep");
    let outcome = tokio::time::timeout(Duration::from_secs(10), sweep)
        .await
        .expect("bounded sweep")
        .expect("sweep task")
        .expect("sweep succeeds");
    outcome
        .flagged
        .iter()
        .map(|f| f.member_id.clone())
        .collect()
}

#[tokio::test]
async fn member_who_became_active_after_selection_is_not_flagged() {
    let Some((pool, schema)) = test_pool("inact_reval_active")
        .await
        .expect("test database setup")
    else {
        eprintln!("skipping inactivity revalidation test: TWO_TEST_DATABASE_URL not set");
        return;
    };
    seed_quiet_member(&pool, STALE).await;
    seed_quiet_member(&pool, QUIET).await;
    let outcome_ids = run_paused_sweep(
        &pool,
        Some(&format!(
            "UPDATE members SET last_active_at = '{AFTER_CUTOFF}'::timestamptz
              WHERE guild_id = '{GUILD}' AND member_id = '{STALE}'"
        )),
    )
    .await;
    let observed = observe(&pool, outcome_ids).await;
    drop_schema(&pool, &schema).await;

    assert!(
        !observed.outcome_ids.iter().any(|id| id == STALE),
        "stale candidate must not appear in the flagged outcome: {:?}",
        observed.outcome_ids
    );
    assert_eq!(observed.events_for_stale, 0, "no event from stale evidence");
    assert_eq!(
        observed.stale_flagged_at, None,
        "no flag timestamp mutation for a skipped stale candidate"
    );
    // The genuinely still-quiet control keeps the existing contract.
    assert!(
        observed.outcome_ids.iter().any(|id| id == QUIET),
        "eligible control is still flagged: {:?}",
        observed.outcome_ids
    );
    assert_eq!(observed.events_total, 1, "exactly the control's event");
}

#[tokio::test]
async fn member_who_left_after_selection_is_not_flagged() {
    let Some((pool, schema)) = test_pool("inact_reval_left")
        .await
        .expect("test database setup")
    else {
        eprintln!("skipping inactivity revalidation test: TWO_TEST_DATABASE_URL not set");
        return;
    };
    seed_quiet_member(&pool, STALE).await;
    let outcome_ids = run_paused_sweep(
        &pool,
        Some(&format!(
            "UPDATE members SET left_at = '{AFTER_CUTOFF}'::timestamptz
              WHERE guild_id = '{GUILD}' AND member_id = '{STALE}'"
        )),
    )
    .await;
    let observed = observe(&pool, outcome_ids).await;
    drop_schema(&pool, &schema).await;

    assert!(
        observed.outcome_ids.is_empty(),
        "departed member must not be flagged: {:?}",
        observed.outcome_ids
    );
    assert_eq!(observed.events_total, 0);
    assert_eq!(observed.stale_flagged_at, None);
}

#[tokio::test]
async fn member_who_became_a_bot_after_selection_is_not_flagged() {
    let Some((pool, schema)) = test_pool("inact_reval_bot")
        .await
        .expect("test database setup")
    else {
        eprintln!("skipping inactivity revalidation test: TWO_TEST_DATABASE_URL not set");
        return;
    };
    seed_quiet_member(&pool, STALE).await;
    let outcome_ids = run_paused_sweep(
        &pool,
        Some(&format!(
            "UPDATE members SET is_bot = TRUE
              WHERE guild_id = '{GUILD}' AND member_id = '{STALE}'"
        )),
    )
    .await;
    let observed = observe(&pool, outcome_ids).await;
    drop_schema(&pool, &schema).await;

    assert!(
        observed.outcome_ids.is_empty(),
        "excluded member must not be flagged: {:?}",
        observed.outcome_ids
    );
    assert_eq!(observed.events_total, 0);
    assert_eq!(observed.stale_flagged_at, None);
}

#[tokio::test]
async fn member_flagged_after_selection_is_not_flagged_again() {
    let Some((pool, schema)) = test_pool("inact_reval_flagged")
        .await
        .expect("test database setup")
    else {
        eprintln!("skipping inactivity revalidation test: TWO_TEST_DATABASE_URL not set");
        return;
    };
    seed_quiet_member(&pool, STALE).await;
    // Another actor flags the same quiet spell after our sweep selected.
    let outcome_ids = run_paused_sweep(
        &pool,
        Some(&format!(
            "UPDATE members SET inactive_flagged_at = '{AFTER_CUTOFF}'::timestamptz
              WHERE guild_id = '{GUILD}' AND member_id = '{STALE}'"
        )),
    )
    .await;
    let observed = observe(&pool, outcome_ids).await;
    drop_schema(&pool, &schema).await;

    assert!(
        observed.outcome_ids.is_empty(),
        "already-flagged quiet spell must not be flagged again: {:?}",
        observed.outcome_ids
    );
    assert_eq!(observed.events_total, 0, "no duplicate event for one spell");
    assert_eq!(
        observed.stale_flagged_at.as_deref(),
        Some(AFTER_CUTOFF),
        "the interim flag stamp is left untouched, not restamped to sweep time"
    );
}

#[tokio::test]
async fn member_who_opted_out_after_selection_is_not_flagged() {
    // Privacy suppression is rechecked inside the per-member transaction
    // today; this control pins that it stays effective through the
    // revalidation change.
    let Some((pool, schema)) = test_pool("inact_reval_optout")
        .await
        .expect("test database setup")
    else {
        eprintln!("skipping inactivity revalidation test: TWO_TEST_DATABASE_URL not set");
        return;
    };
    seed_quiet_member(&pool, STALE).await;
    let outcome_ids = run_paused_sweep(
        &pool,
        Some(&format!(
            "INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ('{GUILD}', '{STALE}')"
        )),
    )
    .await;
    let observed = observe(&pool, outcome_ids).await;
    drop_schema(&pool, &schema).await;

    assert!(observed.outcome_ids.is_empty());
    assert_eq!(observed.events_total, 0);
    assert_eq!(observed.stale_flagged_at, None);
}

#[tokio::test]
async fn still_quiet_member_is_flagged_when_nothing_changes() {
    // Control: the blocker itself changes nothing, so the paused sweep
    // must produce exactly the unpaused contract — one event, the flag
    // stamp at sweep time, and the member in the outcome. A same-outcome
    // re-run at the same sweep time then stays idempotent.
    let Some((pool, schema)) = test_pool("inact_reval_control")
        .await
        .expect("test database setup")
    else {
        eprintln!("skipping inactivity revalidation test: TWO_TEST_DATABASE_URL not set");
        return;
    };
    seed_quiet_member(&pool, STALE).await;
    let outcome_ids = run_paused_sweep(&pool, None).await;
    let observed = observe(&pool, outcome_ids).await;
    let rerun = run_sweep(&pool, NOW, DAYS)
        .await
        .expect("idempotent re-run");
    let rerun_events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE event_type = 'member_inactive'")
            .fetch_one(&pool)
            .await
            .expect("event count after re-run");
    drop_schema(&pool, &schema).await;

    assert_eq!(observed.outcome_ids, [STALE]);
    assert_eq!(observed.events_for_stale, 1);
    assert_eq!(observed.events_total, 1);
    assert_eq!(observed.stale_flagged_at.as_deref(), Some(NOW));
    assert!(rerun.flagged.is_empty(), "re-run flags nobody new");
    assert_eq!(rerun_events, 1, "re-run writes no duplicate event");
}

#[tokio::test]
async fn concurrent_sweeps_flag_one_quiet_spell_once() {
    let Some((pool, schema)) = test_pool("inact_reval_concurrent")
        .await
        .expect("test database setup")
    else {
        eprintln!("skipping inactivity revalidation test: TWO_TEST_DATABASE_URL not set");
        return;
    };
    seed_quiet_member(&pool, STALE).await;
    let mut blocker = pool.begin().await.expect("fixture transaction");
    sqlx::query("LOCK TABLE events IN SHARE MODE")
        .execute(&mut *blocker)
        .await
        .expect("fixture holds the events gate");
    // Different sweep timestamps mean different idempotency keys, so the
    // keys alone cannot dedupe these two sweeps; revalidation must.
    let pool_a = pool.clone();
    let sweep_a = tokio::spawn(async move { run_sweep(&pool_a, NOW, DAYS).await });
    let pool_b = pool.clone();
    let sweep_b = tokio::spawn(async move { run_sweep(&pool_b, NOW_PLUS_MINUTE, DAYS).await });
    // Both blocked at the per-member table lock proves both already
    // selected the member from their initial query.
    wait_for_sweeps_blocked_on_events_lock(&pool, 2).await;
    blocker.commit().await.expect("release both sweeps");
    let out_a = tokio::time::timeout(Duration::from_secs(10), sweep_a)
        .await
        .expect("bounded sweep A")
        .expect("sweep A task")
        .expect("sweep A succeeds");
    let out_b = tokio::time::timeout(Duration::from_secs(10), sweep_b)
        .await
        .expect("bounded sweep B")
        .expect("sweep B task")
        .expect("sweep B succeeds");
    let mut outcome_ids: Vec<String> = out_a
        .flagged
        .iter()
        .chain(out_b.flagged.iter())
        .map(|f| f.member_id.clone())
        .collect();
    outcome_ids.sort();
    let observed = observe(&pool, outcome_ids).await;
    drop_schema(&pool, &schema).await;

    assert_eq!(
        observed.events_total, 1,
        "one quiet spell gets exactly one member_inactive event across both sweeps"
    );
    assert_eq!(
        observed.outcome_ids,
        [STALE],
        "exactly one of the two sweep outcomes reports the member"
    );
    assert!(
        observed.stale_flagged_at.as_deref() == Some(NOW)
            || observed.stale_flagged_at.as_deref() == Some(NOW_PLUS_MINUTE),
        "the winning sweep's stamp stands: {:?}",
        observed.stale_flagged_at
    );
}
