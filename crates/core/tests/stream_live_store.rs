#![cfg(feature = "db")]

//! Durable go-live dedup ledger tests. Database cases are `#[ignore]`d and
//! run only against the disposable `agent-testdb` container (see
//! `crates/core/tests/feeds_store.rs` for the connection-guard pattern);
//! the guard test below runs on every build.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, Pool, Postgres};
use two_bot_core::stream_live_store::*;

/// Test-container-only connection guard: validate the EFFECTIVE parsed
/// options, because sqlx applies query-string overrides while parsing. A
/// Unix-socket override would divert setup/cleanup outside the disposable
/// container, so sockets are rejected outright, and the documented empty
/// test password is forced so an inherited `PGPASSWORD`/`.pgpass`
/// credential can never leak into test connections.
fn test_options(url: &str) -> Result<PgConnectOptions, &'static str> {
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
    Ok(options.password(""))
}

#[test]
fn live_test_options_require_the_approved_container() {
    assert!(test_options("postgres://agent_test:@agent-testdb:5432/agent_test").is_ok());
    for url in [
        "postgres://agent_test@production.example/testdb",
        "postgres://agent_test:@agent-testdb.production.example/agent_test",
        "postgres://agent_test:@localhost/agent_test",
        "postgres://agent_test:@127.0.0.1/agent_test",
        "postgres://agent_test:@agent-testdb/agent_test?host=/var/run/postgresql",
        "postgres://other_user:@agent-testdb/agent_test",
        "postgres://agent_test:@agent-testdb/production",
        "postgres://agent_test:fixture-password@agent-testdb/agent_test",
    ] {
        assert!(
            test_options(url).is_err(),
            "unsafe test options were accepted"
        );
    }
}

async fn connect(schema: &str) -> Pool<Postgres> {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test database URL required");
    let options = test_options(&url).expect("only documented agent-testdb options are allowed");
    PgPoolOptions::new()
        .max_connections(5)
        .connect_with(options.options([("search_path", schema)]))
        .await
        .expect("connect to test container")
}

async fn setup() -> (Pool<Postgres>, String) {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let schema = format!(
        "stream_live_{}_{}_{}",
        std::process::id(),
        nonce,
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    assert!(safe_schema(&schema));
    let pool = connect("public").await;
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let pool = connect(&schema).await;
    // The ledger migrations are `IF NOT EXISTS` throughout: run each twice
    // to prove re-runnability inside one schema.
    for migration in [
        "../../cutover/migrations/0440_stream_live_claims.sql",
        "../../cutover/migrations/0443_stream_live_send_started.sql",
    ] {
        // `include_str!` needs a literal path, so match the two known files.
        if migration.ends_with("0440_stream_live_claims.sql") {
            sqlx::raw_sql(include_str!(
                "../../cutover/migrations/0440_stream_live_claims.sql"
            ))
            .execute(&pool)
            .await
            .unwrap();
            sqlx::raw_sql(include_str!(
                "../../cutover/migrations/0440_stream_live_claims.sql"
            ))
            .execute(&pool)
            .await
            .unwrap();
        } else {
            sqlx::raw_sql(include_str!(
                "../../cutover/migrations/0443_stream_live_send_started.sql"
            ))
            .execute(&pool)
            .await
            .unwrap();
            sqlx::raw_sql(include_str!(
                "../../cutover/migrations/0443_stream_live_send_started.sql"
            ))
            .execute(&pool)
            .await
            .unwrap();
        }
    }
    (pool, schema)
}

fn safe_schema(schema: &str) -> bool {
    schema.len() <= 63
        && schema.starts_with("stream_live_")
        && schema
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

async fn cleanup(pool: Pool<Postgres>, schema: &str) {
    assert!(safe_schema(schema));
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// Extract the ownership token from a winning claim, failing the test on
/// any other outcome.
fn claimed(outcome: ClaimOutcome) -> String {
    match outcome {
        ClaimOutcome::Claimed(token) => token.expose().clone(),
        other => panic!("expected Claimed, got {other:?}"),
    }
}

fn claim(key: &str) -> LiveClaim {
    LiveClaim {
        dedup_key: key.to_owned(),
        broadcaster_id: "123456".to_owned(),
        started_at: "2026-10-12T20:00:00Z".to_owned(),
        channel_id: "100000000000000003".to_owned(),
        content: "LIVE: test is live on Twitch!".to_owned(),
    }
}

#[tokio::test]
#[ignore = "needs the disposable agent-testdb container"]
async fn claim_is_owned_once_then_in_flight() {
    let (pool, schema) = setup().await;
    let key = format!("{:064x}", 0x9e17bfa9a053696fu64);
    claimed(claim_live(&pool, &claim(&key)).await.expect("claim"));
    // The composed receiver entry point: a second delivery of the same
    // fresh never-started claim is InFlight (5xx), not Duplicate (200).
    assert_eq!(
        claim_live(&pool, &claim(&key)).await.expect("retry"),
        ClaimOutcome::InFlight
    );
    // A new instant is a new go-live.
    let mut later = claim(&key);
    later.dedup_key = format!("{:064x}", 0xdeadbeefu64);
    later.started_at = "2026-10-19T20:00:00Z".to_owned();
    claimed(claim_live(&pool, &later).await.expect("new instant"));
    cleanup(pool, &schema).await;
}

#[tokio::test]
#[ignore = "needs the disposable agent-testdb container"]
async fn failed_send_releases_the_claim_for_the_retry() {
    let (pool, schema) = setup().await;
    let key = format!("{:064x}", 0x9e17bfa9a053696fu64);
    let token = claimed(claim_live(&pool, &claim(&key)).await.expect("claim"));
    // Only the token-holding owner can release its never-started row.
    assert!(
        !release_live(&pool, &key, "stale-owner-token")
            .await
            .expect("release wrong token"),
        "releasing with a foreign token must report false"
    );
    assert!(release_live(&pool, &key, &token).await.expect("release"));
    let retry_token = claimed(claim_live(&pool, &claim(&key)).await.expect("retry"));
    assert_ne!(retry_token, token, "a fresh claim must mint a new token");
    assert!(
        !release_live(&pool, &format!("{:064x}", 0u64), &retry_token)
            .await
            .expect("release missing"),
        "releasing an unknown key must report false"
    );
    cleanup(pool, &schema).await;
}

async fn age_claim(pool: &Pool<Postgres>, key: &str, seconds_ago: i64) {
    sqlx::query(
        "UPDATE stream_live_claims SET claimed_at = clock_timestamp() - make_interval(secs => $2) WHERE dedup_key = $1",
    )
    .bind(key)
    .bind(seconds_ago as f64)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs the disposable agent-testdb container"]
async fn fresh_claim_cannot_be_reclaimed() {
    let (pool, schema) = setup().await;
    let key = format!("{:064x}", 0x51e551u64);
    let token = claimed(claim_live(&pool, &claim(&key)).await.expect("claim"));
    // Fresh (unexpired) unposted claim: the retry is InFlight, so the
    // receiver answers 5xx, asking Twitch to retry instead of treating
    // the go-live as delivered. Both the direct reclaim and the composed
    // `claim_live` entry point must give that same answer.
    assert_eq!(
        reclaim_stale_live(&pool, &claim(&key), STALE_CLAIM_LEASE_SECS)
            .await
            .expect("reclaim fresh"),
        ClaimOutcome::InFlight
    );
    assert_eq!(
        claim_live(&pool, &claim(&key)).await.expect("retry"),
        ClaimOutcome::InFlight
    );
    // Once the owner starts sending, the same row is permanently fenced:
    // retries are Duplicate (200), never InFlight, through both calls.
    // A foreign token cannot start the row first.
    assert!(
        !mark_live_send_started(&pool, &key, "stale-owner-token")
            .await
            .expect("start wrong token"),
        "starting with a foreign token must report false"
    );
    assert!(mark_live_send_started(&pool, &key, &token)
        .await
        .expect("start"));
    assert_eq!(
        reclaim_stale_live(&pool, &claim(&key), STALE_CLAIM_LEASE_SECS)
            .await
            .expect("reclaim started"),
        ClaimOutcome::Duplicate
    );
    assert_eq!(
        claim_live(&pool, &claim(&key))
            .await
            .expect("retry started"),
        ClaimOutcome::Duplicate
    );
    cleanup(pool, &schema).await;
}

#[tokio::test]
#[ignore = "needs the disposable agent-testdb container"]
async fn stale_unposted_claim_is_taken_over_atomically() {
    let (pool, schema) = setup().await;
    let key = format!("{:064x}", 0x57a1eu64);
    let old_token = claimed(claim_live(&pool, &claim(&key)).await.expect("claim"));
    age_claim(&pool, &key, STALE_CLAIM_LEASE_SECS + 60).await;
    // Crash-before-send: never started, lease expired, so the retry owns
    // it — through the composed `claim_live` entry point alone.
    let new_token = claimed(claim_live(&pool, &claim(&key)).await.expect("takeover"));
    assert_ne!(new_token, old_token, "takeover must rotate the claim token");
    // The fenced-out original owner can no longer start, release or stamp
    // the row the new owner holds.
    assert!(
        !mark_live_send_started(&pool, &key, &old_token)
            .await
            .expect("start old owner"),
        "a stale owner must not start a taken-over claim"
    );
    assert!(
        !release_live(&pool, &key, &old_token)
            .await
            .expect("release old owner"),
        "a stale owner must not release a taken-over claim"
    );
    assert!(
        !mark_live_delivered(&pool, &key, &old_token, "100000000000000006")
            .await
            .expect("stamp old owner"),
        "a stale owner must not stamp a taken-over claim"
    );
    // Takeover refreshes the lease: an immediate second retry loses and is
    // InFlight (fresh, never started), so it is answered 5xx, not 200.
    assert_eq!(
        reclaim_stale_live(&pool, &claim(&key), STALE_CLAIM_LEASE_SECS)
            .await
            .expect("second reclaim"),
        ClaimOutcome::InFlight
    );
    assert_eq!(
        claim_live(&pool, &claim(&key)).await.expect("retry"),
        ClaimOutcome::InFlight
    );
    // The new owner completes the normal path: start, then stamp.
    assert!(mark_live_send_started(&pool, &key, &new_token)
        .await
        .expect("start"));
    assert!(
        mark_live_delivered(&pool, &key, &new_token, "100000000000000006")
            .await
            .expect("stamp")
    );
    cleanup(pool, &schema).await;
}

#[tokio::test]
#[ignore = "needs the disposable agent-testdb container"]
async fn started_claim_is_never_reclaimed_even_when_stale() {
    let (pool, schema) = setup().await;
    let key = format!("{:064x}", 0x9ead1e55u64);
    let token = claimed(claim_live(&pool, &claim(&key)).await.expect("claim"));
    assert!(mark_live_send_started(&pool, &key, &token)
        .await
        .expect("start"));
    assert!(
        !mark_live_send_started(&pool, &key, &token)
            .await
            .expect("restart"),
        "send start must be idempotent"
    );
    // A started row can never be released, even by its owner: after a
    // stamp failure the send may already have reached Discord, so the
    // row stays fenced instead of freeing a retry to double-post.
    assert!(
        !release_live(&pool, &key, &token)
            .await
            .expect("release started"),
        "a started row must survive release"
    );
    // Send-succeeded/stamp-failed: Discord may already have the post, so
    // ageing the row must NOT make it reclaimable.
    age_claim(&pool, &key, STALE_CLAIM_LEASE_SECS + 3600).await;
    assert_eq!(
        reclaim_stale_live(&pool, &claim(&key), STALE_CLAIM_LEASE_SECS)
            .await
            .expect("reclaim started"),
        ClaimOutcome::Duplicate
    );
    assert_eq!(
        claim_live(&pool, &claim(&key)).await.expect("retry"),
        ClaimOutcome::Duplicate
    );
    // The ambiguous row stays stampable by its owner; it never auto-frees.
    assert!(
        mark_live_delivered(&pool, &key, &token, "100000000000000006")
            .await
            .expect("late stamp")
    );
    cleanup(pool, &schema).await;
}

#[tokio::test]
#[ignore = "needs the disposable agent-testdb container"]
async fn delivered_rows_are_never_reclaimed() {
    let (pool, schema) = setup().await;
    let key = format!("{:064x}", 0x9e17bfa9a053696fu64);
    let token = claimed(claim_live(&pool, &claim(&key)).await.expect("claim"));
    assert!(
        !mark_live_delivered(&pool, &key, "stale-owner-token", "100000000000000006")
            .await
            .expect("stamp wrong token"),
        "stamping with a foreign token must report false"
    );
    assert!(
        mark_live_delivered(&pool, &key, &token, "100000000000000006")
            .await
            .expect("stamp")
    );
    assert!(
        !release_live(&pool, &key, &token).await.expect("release"),
        "a delivered row must survive release"
    );
    assert_eq!(
        claim_live(&pool, &claim(&key)).await.expect("replay"),
        ClaimOutcome::Duplicate
    );
    assert!(
        !mark_live_delivered(&pool, &key, &token, "100000000000000007")
            .await
            .expect("restamp"),
        "a delivered row must not restamp"
    );
    // Ageing a delivered fence must never make it reclaimable.
    age_claim(&pool, &key, STALE_CLAIM_LEASE_SECS + 3600).await;
    assert_eq!(
        reclaim_stale_live(&pool, &claim(&key), STALE_CLAIM_LEASE_SECS)
            .await
            .expect("reclaim delivered"),
        ClaimOutcome::Duplicate
    );
    assert!(
        !mark_live_send_started(&pool, &key, &token)
            .await
            .expect("start delivered"),
        "a delivered row must not start sending"
    );
    cleanup(pool, &schema).await;
}
