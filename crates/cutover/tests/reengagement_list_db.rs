//! Same effective-options guard as raid_list_db tests; owned schema only.
//! The reengagement query is the inactivity selector's quiet members for one
//! guild: row-for-row the sweep's WHERE clause, guild-scoped, read-only
//! (no `member_inactive` events, no `inactive_flagged_at` projection).
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions,
};
use std::str::FromStr;
use two_bot_core::inactivity::{
    parse_reengagement_csv, render_reengagement_csv, InactivityOutcome,
};
use two_bot_cutover::reengagement::list_reengagement;

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

const GUILD: &str = "100000000000000010";
const OTHER_GUILD: &str = "100000000000000011";
const NOW: &str = "2026-09-07T00:00:00.000Z";

async fn seed_member(
    pool: &sqlx::PgPool,
    guild: &str,
    member: &str,
    joined_at: Option<&str>,
    last_active_at: Option<&str>,
    flagged_at: Option<&str>,
    is_bot: bool,
    left_at: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO members (guild_id, member_id, joined_at, last_active_at, inactive_flagged_at, is_bot, left_at)
         VALUES ($1, $2, $3::timestamptz, $4::timestamptz, $5::timestamptz, $6, $7::timestamptz)",
    )
    .bind(guild)
    .bind(member)
    .bind(joined_at)
    .bind(last_active_at)
    .bind(flagged_at)
    .bind(is_bot)
    .bind(left_at)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires explicit agent-testdb / CI service-container URL"]
async fn list_is_guild_scoped_selector_shaped_and_read_only() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test-container URL required");
    let options = test_options(&url).expect("refusing non-test database");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .expect("agent-testdb connection");
    let schema = format!(
        "reengage12141_{}_{}",
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
    // Quiet past the 14-day cutoff: listed.
    seed_member(
        &pool,
        GUILD,
        "100000000000000001",
        Some("2026-07-01T00:00:00.000Z"),
        None,
        None,
        false,
        None,
    )
    .await;
    // Quiet 60 days ago but flagged 30 days ago (before the cutoff): a new
    // quiet spell, listed again.
    seed_member(
        &pool,
        GUILD,
        "100000000000000002",
        Some("2026-06-01T00:00:00.000Z"),
        Some("2026-07-01T00:00:00.000Z"),
        Some("2026-08-08T00:00:00.000Z"),
        false,
        None,
    )
    .await;
    // Active yesterday: not listed.
    seed_member(
        &pool,
        GUILD,
        "100000000000000003",
        Some("2026-07-01T00:00:00.000Z"),
        Some("2026-09-06T00:00:00.000Z"),
        None,
        false,
        None,
    )
    .await;
    // Quiet but a bot / departed / flagged-this-spell: not listed.
    seed_member(
        &pool,
        GUILD,
        "100000000000000004",
        Some("2026-07-01T00:00:00.000Z"),
        None,
        None,
        true,
        None,
    )
    .await;
    seed_member(
        &pool,
        GUILD,
        "100000000000000005",
        Some("2026-07-01T00:00:00.000Z"),
        None,
        None,
        false,
        Some("2026-08-01T00:00:00.000Z"),
    )
    .await;
    seed_member(
        &pool,
        GUILD,
        "100000000000000006",
        Some("2026-07-01T00:00:00.000Z"),
        None,
        Some("2026-09-06T00:00:00.000Z"),
        false,
        None,
    )
    .await;
    // Quiet in another guild: guild-scoped out.
    seed_member(
        &pool,
        OTHER_GUILD,
        "100000000000000007",
        Some("2026-07-01T00:00:00.000Z"),
        None,
        None,
        false,
        None,
    )
    .await;
    // Never seen at all (neither activity nor join): SQL NULL logic excludes.
    seed_member(
        &pool,
        GUILD,
        "100000000000000008",
        None,
        None,
        None,
        false,
        None,
    )
    .await;

    let rows = list_reengagement(&pool, GUILD, NOW, 14).await.unwrap();
    let ids: Vec<_> = rows.iter().map(|r| r.member_id.as_str()).collect();
    assert_eq!(ids, ["100000000000000001", "100000000000000002"]);
    assert!(rows
        .iter()
        .all(|r| r.guild_id == GUILD && r.occurred_at == NOW && r.threshold_days == 14));
    // Row-for-row CSV acceptance: the DB rows render and parse back exactly.
    let csv = render_reengagement_csv(&InactivityOutcome {
        flagged: rows.clone(),
    })
    .unwrap();
    assert_eq!(parse_reengagement_csv(&csv).unwrap(), rows);
    // Empty scope: a guild with no member rows renders the header plus hint.
    // (OTHER_GUILD holds quiet member ...007, so it lists one row — that is
    // the guild-scoping check, not the empty check.)
    let other = list_reengagement(&pool, OTHER_GUILD, NOW, 14)
        .await
        .unwrap();
    assert_eq!(other.len(), 1);
    assert_eq!(other[0].member_id, "100000000000000007");
    let empty = list_reengagement(&pool, "100000000000000012", NOW, 14)
        .await
        .unwrap();
    assert!(empty.is_empty());
    let empty_csv = render_reengagement_csv(&InactivityOutcome { flagged: empty }).unwrap();
    assert_eq!(
        empty_csv.lines().next().unwrap(),
        "guild_id,member_id,occurred_at,threshold_days"
    );
    assert!(parse_reengagement_csv(&empty_csv).unwrap().is_empty());
    // Bad timestamp is a query error, not a panic.
    assert!(list_reengagement(&pool, GUILD, "invalid", 14)
        .await
        .is_err());
    // Read-only: no events recorded, no flags projected.
    let (events, flagged): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM events), (SELECT COUNT(*) FROM members WHERE inactive_flagged_at IS NOT NULL)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (events, flagged),
        (0, 1),
        "no events recorded; only the seeded flag row is projected"
    );
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
