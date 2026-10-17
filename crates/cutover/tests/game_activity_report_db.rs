//! DB-fixture acceptance for the read-only aggregate game-activity operator
//! report: seeded `game_activity_guild_aggregates` rows, read back through
//! the store reader the bin uses (`read_guild_aggregates`) and rendered by
//! the core report renderer, must produce the exact pinned JSON rows —
//! guild-isolated, untitled-aware, and with no writes.
//!
//! Template: `reengagement_list_db.rs` (same guarded test options, owned
//! schema per run). The database test skips at runtime when
//! `TWO_TEST_DATABASE_URL` is absent (the `gate_stuck_report.rs` pattern)
//! and refuses every non-test-container target when it is present. Locally:
//! `TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/agent_test
//! python3 scripts/cargo_cache.py run -- test -p two-bot-cutover
//! --test game_activity_report_db`
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions,
};
use std::str::FromStr;
use two_bot_core::game_activity::render_game_activity_report;
use two_bot_core::game_activity_store::read_guild_aggregates;

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
const EMPTY_GUILD: &str = "100000000000000012";
const MIN_MS: i64 = 60_000;

async fn seed(pool: &sqlx::PgPool, guild: &str, game: &str, total: i64, voice: i64, sessions: i64) {
    sqlx::query(
        "INSERT INTO game_activity_guild_aggregates
             (guild_id, game, total_play_ms, in_voice_ms, session_count)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(guild)
    .bind(game)
    .bind(total)
    .bind(voice)
    .bind(sessions)
    .execute(pool)
    .await
    .unwrap();
}

/// Whole-table witness: every stored column of every row, order-stable.
/// A read-only report leaves this byte-identical (counts, totals and
/// `updated_at` alike).
async fn table_witness(pool: &sqlx::PgPool) -> serde_json::Value {
    sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY t.guild_id, t.game), '[]'::jsonb)
           FROM game_activity_guild_aggregates t",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn seeded_aggregates_render_exactly_guild_scoped_and_read_only() {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        eprintln!("skipping game_activity_report_db test: TWO_TEST_DATABASE_URL not set");
        return;
    };
    let options = test_options(&url).expect("refusing non-test database");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .expect("agent-testdb connection");
    let schema = format!(
        "game_report28012_{}_{}",
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
    sqlx::raw_sql(include_str!(
        "../migrations/0441_game_activity_guild_aggregates.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();

    // Seed in stored form: untitled is the empty-string key (migration 0441).
    seed(&pool, GUILD, "Apex", 40 * MIN_MS, MIN_MS, 3).await;
    seed(&pool, GUILD, "", MIN_MS, 0, 1).await;
    seed(&pool, GUILD, "Zelda", 40 * MIN_MS, 2 * MIN_MS, 2).await;
    seed(&pool, OTHER_GUILD, "Apex", 99 * MIN_MS, 0, 9).await;
    seed(&pool, OTHER_GUILD, "OtherGuildOnlyGame", 5 * MIN_MS, 0, 1).await;
    let seeded = table_witness(&pool).await;
    let (seeded_rows,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM game_activity_guild_aggregates")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(seeded_rows, 5);

    // The bin's pipeline: guild-scoped store read, then the core renderer.
    let rows = read_guild_aggregates(&pool, GUILD).await.unwrap();
    let report = render_game_activity_report(GUILD, &rows);
    let parsed: serde_json::Value = serde_json::from_str(&report).expect("report is JSON");
    assert_eq!(parsed["guild_id"], GUILD);
    assert_eq!(parsed["truncated"], false);
    assert_eq!(
        parsed["rows"],
        serde_json::json!([
            {"game": "Zelda", "total_play_ms": 40 * MIN_MS, "in_voice_ms": 2 * MIN_MS, "session_count": 2},
            {"game": "Apex", "total_play_ms": 40 * MIN_MS, "in_voice_ms": MIN_MS, "session_count": 3},
            {"game": "(untitled)", "total_play_ms": MIN_MS, "in_voice_ms": 0, "session_count": 1},
        ]),
        "seeded aggregates render exact per-game rows; '' maps to the placeholder"
    );
    // Guild isolation at the report surface: nothing from the other guild —
    // neither its unique game, its larger Apex totals, nor its id.
    assert!(!report.contains("OtherGuildOnlyGame"), "{report}");
    assert!(!report.contains(OTHER_GUILD), "{report}");
    assert!(
        !report.contains("99"),
        "other guild totals leaked: {report}"
    );

    // The other guild's own report sees only its own rows.
    let other_rows = read_guild_aggregates(&pool, OTHER_GUILD).await.unwrap();
    let other: serde_json::Value =
        serde_json::from_str(&render_game_activity_report(OTHER_GUILD, &other_rows)).unwrap();
    assert_eq!(other["rows"].as_array().unwrap().len(), 2);
    assert_eq!(other["rows"][0]["game"], "Apex");
    assert_eq!(other["rows"][0]["total_play_ms"], 99 * MIN_MS);

    // Empty guild: header shape plus hint (the CLI test pins exit 0).
    let empty_rows = read_guild_aggregates(&pool, EMPTY_GUILD).await.unwrap();
    assert!(empty_rows.is_empty());
    let empty: serde_json::Value =
        serde_json::from_str(&render_game_activity_report(EMPTY_GUILD, &empty_rows)).unwrap();
    assert_eq!(empty["guild_id"], EMPTY_GUILD);
    assert!(empty["rows"].as_array().unwrap().is_empty());
    assert_eq!(empty["hint"], "no game activity recorded for this guild");

    // No-write proof: after three report runs the table is byte-identical —
    // same row count, same totals, same updated_at stamps.
    assert_eq!(table_witness(&pool).await, seeded, "report wrote rows");
    let (after_rows,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM game_activity_guild_aggregates")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(after_rows, seeded_rows);

    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
