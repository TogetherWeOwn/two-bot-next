//! Same effective-options guard as internal_action_store tests; owned schema only.
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions,
};
use std::str::FromStr;
use two_bot_cutover::raid_tools::list_flagged;

fn test_options(url: &str) -> Result<PgConnectOptions, &'static str> {
    if url.contains(['?', '#']) {
        return Err("no test URL overrides");
    }
    if !(url.starts_with("postgres://agent_test:@") || url.starts_with("postgresql://agent_test:@"))
    {
        return Err("explicit empty test password required");
    }
    let options = PgConnectOptions::from_str(url).map_err(|_| "invalid test URL")?;
    if options.get_host() != "agent-testdb"
        || options.get_port() != 5432
        || options.get_socket().is_some()
        || options.get_username() != "agent_test"
        || options.get_options().is_some()
        || options.get_database() != Some("agent_test")
        || options
            .to_url_lossy()
            .password()
            .is_some_and(|p| !p.is_empty())
    {
        return Err("test-container target only");
    }
    Ok(options.password(""))
}

#[test]
fn database_guard_refuses_staging_production_and_override_targets() {
    assert!(test_options("postgres://agent_test:@agent-testdb:5432/agent_test").is_ok());
    for url in [
        "postgres://agent_test:@staging:5432/agent_test",
        "postgres://agent_test:@agent-testdb:5432/production",
        "postgres://agent_test:secret@agent-testdb:5432/agent_test",
        "postgres://agent_test:@agent-testdb:5432/agent_test?host=production",
        "postgres://agent_test:@agent-testdb:5432/agent_test#fragment",
    ] {
        assert!(test_options(url).is_err());
    }
}

#[tokio::test]
#[ignore = "requires explicit agent-testdb / CI service-container URL"]
async fn flags_are_guild_scoped_half_open_deduplicated_and_activity_protected() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test-container URL required");
    let options = test_options(&url).expect("refusing non-test database");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .expect("agent-testdb connection");
    let schema = format!(
        "raid10867_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    assert!(
        schema.len() <= 63
            && schema
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    );
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |c, _| {
            let path = path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(path)
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0001_funnel.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0360_join_risk_flags.sql"))
        .execute(&pool)
        .await
        .unwrap();
    for (event, member, guild, at, flagged, bulk) in [
        (
            "a",
            "100000000000000001",
            "100000000000000010",
            "2026-09-01T00:00:00Z",
            true,
            false,
        ),
        (
            "duplicate",
            "100000000000000001",
            "100000000000000010",
            "2026-09-01T00:00:01Z",
            true,
            false,
        ),
        (
            "end",
            "100000000000000002",
            "100000000000000010",
            "2026-09-02T00:00:00Z",
            true,
            false,
        ),
        (
            "other",
            "100000000000000003",
            "100000000000000011",
            "2026-09-01T00:00:00Z",
            true,
            false,
        ),
        (
            "unflagged",
            "100000000000000004",
            "100000000000000010",
            "2026-09-01T00:00:00Z",
            false,
            false,
        ),
        (
            "bulk",
            "100000000000000005",
            "100000000000000010",
            "2026-09-01T00:00:00Z",
            true,
            true,
        ),
        (
            "active",
            "100000000000000006",
            "100000000000000010",
            "2026-09-01T00:00:00Z",
            true,
            false,
        ),
        (
            "voice-funnel-only",
            "100000000000000008",
            "100000000000000010",
            "2026-09-01T00:00:00Z",
            true,
            false,
        ),
        (
            "funnel",
            "100000000000000007",
            "100000000000000010",
            "2026-09-01T00:00:00Z",
            true,
            false,
        ),
    ] {
        sqlx::query(
            "INSERT INTO join_risk_flags VALUES ($1,$2,$3,$4,$4,'unknown',3,'[]',$5,$6,$4)",
        )
        .bind(event)
        .bind(guild)
        .bind(member)
        .bind(at)
        .bind(bulk)
        .bind(flagged)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO members (guild_id,member_id,first_voice_at) VALUES ('100000000000000010','100000000000000006',now())").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO events (guild_id,member_id,event_type,occurred_at,source,idempotency_key) VALUES ('100000000000000010','100000000000000007','first_message',now(),'test','test-key')").execute(&pool).await.unwrap();
    // Historical funnel evidence without a members projection must also protect.
    sqlx::query("INSERT INTO events (guild_id,member_id,event_type,occurred_at,source,idempotency_key) VALUES ('100000000000000010','100000000000000008','first_voice_session',now(),'test','voice-test-key')").execute(&pool).await.unwrap();
    let rows = list_flagged(
        &pool,
        "100000000000000010",
        "2026-09-01T00:00:00Z",
        "2026-09-02T00:00:00Z",
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].member_id, "100000000000000001");
    assert!(list_flagged(
        &pool,
        "100000000000000010",
        "invalid",
        "2026-09-02T00:00:00Z"
    )
    .await
    .is_err());
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
