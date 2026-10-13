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
    // The ledger migration is `IF NOT EXISTS` throughout: run it twice to
    // prove re-runnability inside one schema.
    sqlx::raw_sql(include_str!(
        "../../cutover/migrations/0438_stream_live_claims.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../../cutover/migrations/0438_stream_live_claims.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
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
async fn claim_is_owned_once_then_duplicate() {
    let (pool, schema) = setup().await;
    let key = format!("{:064x}", 0x9e17bfa9a053696fu64);
    assert_eq!(
        claim_live(&pool, &claim(&key)).await.expect("claim"),
        ClaimOutcome::Claimed
    );
    assert_eq!(
        claim_live(&pool, &claim(&key)).await.expect("reclaim"),
        ClaimOutcome::Duplicate
    );
    // A new instant is a new go-live.
    let mut later = claim(&key);
    later.dedup_key = format!("{:064x}", 0xdeadbeefu64);
    later.started_at = "2026-10-19T20:00:00Z".to_owned();
    assert_eq!(
        claim_live(&pool, &later).await.expect("new instant"),
        ClaimOutcome::Claimed
    );
    cleanup(pool, &schema).await;
}

#[tokio::test]
#[ignore = "needs the disposable agent-testdb container"]
async fn failed_send_releases_the_claim_for_the_retry() {
    let (pool, schema) = setup().await;
    let key = format!("{:064x}", 0x9e17bfa9a053696fu64);
    assert_eq!(
        claim_live(&pool, &claim(&key)).await.expect("claim"),
        ClaimOutcome::Claimed
    );
    assert!(release_live(&pool, &key).await.expect("release"));
    assert_eq!(
        claim_live(&pool, &claim(&key)).await.expect("retry"),
        ClaimOutcome::Claimed
    );
    assert!(
        !release_live(&pool, &format!("{:064x}", 0u64))
            .await
            .expect("release missing"),
        "releasing an unknown key must report false"
    );
    cleanup(pool, &schema).await;
}

#[tokio::test]
#[ignore = "needs the disposable agent-testdb container"]
async fn delivered_rows_are_never_released() {
    let (pool, schema) = setup().await;
    let key = format!("{:064x}", 0x9e17bfa9a053696fu64);
    assert_eq!(
        claim_live(&pool, &claim(&key)).await.expect("claim"),
        ClaimOutcome::Claimed
    );
    assert!(mark_live_delivered(&pool, &key, "100000000000000006")
        .await
        .expect("stamp"));
    assert!(
        !release_live(&pool, &key).await.expect("release"),
        "a delivered row must survive release"
    );
    assert_eq!(
        claim_live(&pool, &claim(&key)).await.expect("replay"),
        ClaimOutcome::Duplicate
    );
    assert!(
        !mark_live_delivered(&pool, &key, "100000000000000007")
            .await
            .expect("restamp"),
        "a delivered row must not restamp"
    );
    cleanup(pool, &schema).await;
}
