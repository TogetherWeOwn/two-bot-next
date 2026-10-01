//! Explicit opt-in, isolated-schema integration test (agent-testdb only).

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use two_bot_cutover::invite_store::{store_counters, CounterRow};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const NEW: &str = "2026-02-02T00:00:00Z";

fn row(code: &str, uses: i64) -> CounterRow {
    CounterRow {
        code: code.to_owned(),
        uses,
        inviter_id: Some("inviter".to_owned()),
        channel_id: None,
    }
}

async fn snapshot(pool: &PgPool) -> TestResult<Vec<(String, String, i32, String)>> {
    Ok(sqlx::query_as(
        "SELECT guild_id, code, uses, updated_at::text FROM invite_snapshots ORDER BY guild_id, code",
    )
    .fetch_all(pool)
    .await?)
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn counter_checkpoint_is_atomic() -> TestResult {
    let options = PgConnectOptions::new()
        .host("agent-testdb")
        .port(5432)
        .username("agent_test")
        .password("")
        .database("agent_test");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let schema = format!("invite_store_{}_{}", std::process::id(), nonce);
    // Audited: identifier is a fixed prefix plus numeric PID and timestamp.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await?;
    let search_path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(search_path)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await?;
    let result = exercise(&pool).await;
    pool.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await?;
    admin.close().await;
    result
}

async fn exercise(pool: &PgPool) -> TestResult {
    sqlx::raw_sql(
        "CREATE TABLE invite_snapshots (
           guild_id TEXT NOT NULL, code TEXT NOT NULL, uses INTEGER NOT NULL,
           inviter_id TEXT, channel_id TEXT, updated_at timestamptz NOT NULL,
           PRIMARY KEY (guild_id, code));
         INSERT INTO invite_snapshots (guild_id, code, uses, updated_at) VALUES
           ('g1','a',1,'2026-01-01T00:00:00Z'),
           ('g1','b',2,'2026-01-01T00:00:00Z'),
           ('g1','gone',9,'2026-01-01T00:00:00Z'),
           ('g2','a',7,'2026-01-01T00:00:00Z');",
    )
    .execute(pool)
    .await?;
    let before = snapshot(pool).await?;

    // Fail the second upsert (code 'b' uses=20) after 'a' already succeeded.
    sqlx::raw_sql(
        "CREATE FUNCTION fail_b() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF NEW.code = 'b' AND NEW.uses = 20 THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$;
         CREATE TRIGGER fail_b BEFORE UPDATE ON invite_snapshots
           FOR EACH ROW EXECUTE FUNCTION fail_b();",
    )
    .execute(pool)
    .await?;
    let rows = [row("a", 10), row("b", 20)];
    assert!(store_counters(pool, "g1", &rows, NEW).await.is_err());
    assert_eq!(
        snapshot(pool).await?,
        before,
        "failed checkpoint must roll back fully"
    );

    // Retry succeeds once the fault is gone.
    sqlx::raw_sql("DROP TRIGGER fail_b ON invite_snapshots")
        .execute(pool)
        .await?;
    store_counters(pool, "g1", &rows, NEW).await?;
    let after = snapshot(pool).await?;
    let g1: Vec<_> = after.iter().filter(|r| r.0 == "g1").collect();
    assert_eq!(g1.len(), 2, "absent code removed");
    assert!(g1.iter().all(|r| r.3.starts_with("2026-02-02 00:00:00")));
    assert_eq!((g1[0].1.as_str(), g1[0].2), ("a", 10));
    assert_eq!((g1[1].1.as_str(), g1[1].2), ("b", 20));
    let foreign: Vec<_> = before.iter().filter(|r| r.0 == "g2").collect();
    let foreign_after: Vec<_> = after.iter().filter(|r| r.0 == "g2").collect();
    assert_eq!(foreign, foreign_after, "foreign guild untouched");
    Ok(())
}
