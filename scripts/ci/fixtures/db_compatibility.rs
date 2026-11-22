// CI adapter copied unchanged into both source checkouts as a Cargo example.
// The migrations, grant plan and store calls link against that checkout.
use two_bot_core::{database_roles, database_tls::TlsPolicy, scheduled_store};
use two_bot_cutover::{db, raid_tools};

const SOURCE: &str = "postgres://compat_migrate:@agent-testdb:5432/two_bot_test_compat_source";
const TARGET: &str = "postgres://compat_migrate:@agent-testdb:5432/two_bot_test_compat_target";
const RUNTIME: &str = "postgres://compat_runtime:@agent-testdb:5432/two_bot_test_compat_target";
const GUILD: &str = "100000000000000001";

#[tokio::main]
async fn main() -> Result<(), sqlx::Error> {
    assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
    match std::env::args().nth(1).as_deref() {
        Some("plan") => print!("{}", database_roles::plan()),
        Some("source") => migrate(SOURCE).await?,
        Some("target") => migrate(TARGET).await?,
        Some("smoke") => smoke().await?,
        _ => panic!("expected plan, source, target or smoke"),
    }
    Ok(())
}

async fn migrate(url: &str) -> Result<(), sqlx::Error> {
    let conn = db::connect_with_tls(url, 1, true, TlsPolicy::LocalOnly).await?;
    let role: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(conn.pool())
        .await?;
    assert_eq!(role, "two_bot_migrator");
    db::migrate_pool(conn.pool()).await?;
    two_bot_store::migrate(conn.pool()).await?;
    two_bot_store::apply_web_contract(conn.pool()).await?;
    conn.close().await;
    Ok(())
}

async fn smoke() -> Result<(), sqlx::Error> {
    let conn = db::connect_with_tls(RUNTIME, 1, true, TlsPolicy::LocalOnly).await?;
    let role: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(conn.pool())
        .await?;
    assert_eq!(role, "two_bot_runtime");
    let cohort = raid_tools::list_flagged(
        conn.pool(),
        GUILD,
        "2026-01-01T00:00:00Z",
        "2026-01-02T00:00:00Z",
    )
    .await?;
    assert_eq!(cohort.len(), 1, "restored raid evidence");
    assert_eq!(cohort[0].member_id, "100000000000000002");
    let claimed = scheduled_store::claim_due(
        conn.pool(),
        GUILD,
        "2026-01-01T01:00:00.000Z",
        "compat-claim",
        "2026-01-01T02:00:00.000Z",
        "compat-nonce",
    )
    .await?;
    assert_eq!(claimed.len(), 1, "one restored schedule leased");
    assert_eq!(claimed[0].id, "compat-schedule");
    assert_eq!(claimed[0].claim_token.as_deref(), Some("compat-claim"));
    assert_eq!(claimed[0].occurrence_nonce.as_deref(), Some("compat-nonce"));
    conn.close().await;
    println!("PRIOR DB SMOKE VERIFIED");
    Ok(())
}
