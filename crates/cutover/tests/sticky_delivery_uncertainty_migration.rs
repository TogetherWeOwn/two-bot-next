//! Forward/rollback compatibility for the sticky uncertain-delivery fence.
//!
//! The `0454_sticky_delivery_uncertainty.sql` migration adds one
//! `delivery_uncertain` flag to `sticky_messages`: additive, `IF NOT EXISTS`,
//! pre-existing rows read back certain. Applying the full chain on a fresh
//! database does not prove the rollback half, so the disposable-database test
//! below arms the fence, applies the documented `DROP COLUMN` rollback,
//! proves the sticky row survives, then re-applies the checked-in file twice
//! to prove idempotency and default restoration.
//!
//! Unignored with an explicit SKIP: without `TWO_TEST_DATABASE_URL` the
//! database test prints SKIP and returns (CI always configures it, so the
//! suite runs there without a new workflow). The owned disposable database
//! is dropped even when the scenario panics.

use sqlx::PgPool;
use two_bot_testsupport::TestDatabase;

const MIGRATION_FILE: &str = "0454_sticky_delivery_uncertainty.sql";

fn migration_path() -> String {
    format!("{}/migrations/{MIGRATION_FILE}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn uncertainty_migration_file_exists_and_pins_fence_column() {
    let path = migration_path();
    assert!(
        std::path::Path::new(&path).exists(),
        "fence migration must exist at {path}"
    );
    let content = std::fs::read_to_string(&path).expect("migration file must be readable");
    assert!(
        content.contains("delivery_uncertain"),
        "migration must add the delivery_uncertain fence column"
    );
    assert!(
        content.contains("IF NOT EXISTS"),
        "migration must stay re-appliable (IF NOT EXISTS)"
    );
    assert!(
        content.to_ascii_lowercase().contains("rollback") && content.contains("DROP COLUMN"),
        "migration must document its rollback (DROP COLUMN)"
    );
}

async fn scenario(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO sticky_messages
           (guild_id, channel_id, body, created_by, created_at, updated_by, updated_at)
         VALUES ($1, $2, $3, 'admin-1', clock_timestamp(), 'admin-1', clock_timestamp())",
    )
    .bind("g-uncertain")
    .bind("c-uncertain")
    .bind("migration probe sticky")
    .execute(pool)
    .await
    .expect("seed sticky row without touching the fence column");
    let (uncertain,): (bool,) = sqlx::query_as(
        "SELECT delivery_uncertain FROM sticky_messages
          WHERE guild_id = $1 AND channel_id = $2",
    )
    .bind("g-uncertain")
    .bind("c-uncertain")
    .fetch_one(pool)
    .await
    .expect("fence column reads back");
    assert!(!uncertain, "pre-existing rows read back certain");

    sqlx::query(
        "UPDATE sticky_messages SET delivery_uncertain = TRUE
          WHERE guild_id = $1 AND channel_id = $2",
    )
    .bind("g-uncertain")
    .bind("c-uncertain")
    .execute(pool)
    .await
    .expect("arm the fence");

    // The documented rollback: fences are lost, sticky rows survive.
    sqlx::raw_sql("ALTER TABLE sticky_messages DROP COLUMN IF EXISTS delivery_uncertain")
        .execute(pool)
        .await
        .expect("documented rollback applies");
    let (columns,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM information_schema.columns
          WHERE table_name = 'sticky_messages' AND column_name = 'delivery_uncertain'",
    )
    .fetch_one(pool)
    .await
    .expect("column inventory reads back");
    assert_eq!(columns, 0, "rollback drops the fence column");
    let (body,): (String,) =
        sqlx::query_as("SELECT body FROM sticky_messages WHERE guild_id = $1 AND channel_id = $2")
            .bind("g-uncertain")
            .bind("c-uncertain")
            .fetch_one(pool)
            .await
            .expect("sticky row survives the rollback");
    assert_eq!(body, "migration probe sticky");

    // Re-applying the checked-in file twice proves idempotency and restores
    // the certain default on the surviving row.
    let content = std::fs::read_to_string(migration_path()).expect("migration file re-reads");
    for _ in 0..2 {
        // Audited: the file is the reviewed fence migration; applying it to
        // the owned disposable database is this test's purpose.
        sqlx::raw_sql(sqlx::AssertSqlSafe(content.clone()))
            .execute(pool)
            .await
            .expect("checked-in migration re-applies");
    }
    let (uncertain,): (bool,) = sqlx::query_as(
        "SELECT delivery_uncertain FROM sticky_messages
          WHERE guild_id = $1 AND channel_id = $2",
    )
    .bind("g-uncertain")
    .bind("c-uncertain")
    .fetch_one(pool)
    .await
    .expect("fence column reads back after re-apply");
    assert!(
        !uncertain,
        "re-applied default leaves pre-existing rows certain"
    );
}

#[tokio::test]
async fn uncertainty_migration_round_trip_preserves_sticky_rows() {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the migration round-trip suite"
            );
            eprintln!("SKIP sticky uncertainty migration: TWO_TEST_DATABASE_URL not set");
            return;
        }
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
        .await
        .expect("migrated disposable test database");
    let pool = db.pool().clone();
    let outcome = tokio::spawn(async move { scenario(&pool).await }).await;
    db.close()
        .await
        .expect("drop owned disposable database even on RED");
    match outcome {
        Ok(()) => {}
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => panic!("migration scenario join failed: {error}"),
    }
}
