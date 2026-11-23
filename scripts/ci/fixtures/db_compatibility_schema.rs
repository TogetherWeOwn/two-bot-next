// CI adapter for the checkout's embedded migrations, web schema and grant plan.
use two_bot_core::{database_roles, database_tls::TlsPolicy};
use two_bot_cutover::db;

const SOURCE: &str = "postgres://compat_migrate:@agent-testdb:5432/two_bot_test_compat_source";
const TARGET: &str = "postgres://compat_migrate:@agent-testdb:5432/two_bot_test_compat_target";

#[tokio::main]
async fn main() -> Result<(), sqlx::Error> {
    assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
    match std::env::args().nth(1).as_deref() {
        Some("plan") => print!("{}", database_roles::plan()),
        Some("source") => migrate(SOURCE).await?,
        Some("target") => migrate(TARGET).await?,
        _ => panic!("expected plan, source or target"),
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
