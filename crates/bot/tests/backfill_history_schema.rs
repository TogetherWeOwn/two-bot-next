//! History metadata must exist in the deployment migration chain, not only
//! after `two_bot_store::migrate`. This fixture executes the same cutover
//! migration directory embedded by the deployment migrator. No history DDL is
//! supplied by the test. The bootstrap must be the disposable test database.

use two_bot_cutover::staging_migrate::MIGRATOR;
use two_bot_testsupport::TestDatabase;

#[tokio::test]
async fn cutover_chain_creates_metadata_only_history_tables() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let db = TestDatabase::create(&url, &MIGRATOR)
        .await
        .expect("create cutover-chain fixture");
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables
         WHERE table_schema = 'public'
           AND table_name IN ('backfill_history_messages', 'backfill_history_counts')
         ORDER BY table_name",
    )
    .fetch_all(db.pool())
    .await
    .expect("read deployed table inventory");
    let columns: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT table_name, column_name, data_type FROM information_schema.columns
         WHERE table_schema = 'public'
           AND table_name IN ('backfill_history_messages', 'backfill_history_counts')
         ORDER BY table_name, column_name",
    )
    .fetch_all(db.pool())
    .await
    .expect("read deployed column inventory");
    let primary_keys: Vec<(String, String)> = sqlx::query_as(
        "SELECT t.table_name, k.column_name
         FROM information_schema.table_constraints t
         JOIN information_schema.key_column_usage k
           ON k.constraint_catalog = t.constraint_catalog
          AND k.constraint_schema = t.constraint_schema
          AND k.constraint_name = t.constraint_name
          AND k.table_name = t.table_name
         WHERE t.table_schema = 'public' AND t.constraint_type = 'PRIMARY KEY'
           AND t.table_name IN ('backfill_history_messages', 'backfill_history_counts')
         ORDER BY t.table_name, k.ordinal_position",
    )
    .fetch_all(db.pool())
    .await
    .expect("read replay-safety keys");
    // Evaluate the production allowlist query, without creating shared roles or
    // applying grants. Actual privilege probes remain in database_roles tests.
    let matrix: Vec<(String, String, String)> =
        sqlx::query_as(include_str!("../../../sql/database_role_matrix.sql"))
            .fetch_all(db.pool())
            .await
            .expect("evaluate reviewed role classification");
    let mut history_roles: Vec<_> = matrix
        .into_iter()
        .filter(|(_, name, _)| {
            matches!(
                name.as_str(),
                "backfill_history_messages" | "backfill_history_counts"
            )
        })
        .collect();
    history_roles.sort();
    let mut conn = db
        .pool()
        .acquire()
        .await
        .expect("schema inspection connection");
    let erasure_gaps = two_bot_cutover::member_erasure::schema_gaps(&mut conn)
        .await
        .expect("run production erasure schema checker");
    drop(conn);
    db.close()
        .await
        .expect("drop owned fixture before assertions");

    assert_eq!(
        tables,
        ["backfill_history_counts", "backfill_history_messages"],
        "the actual cutover chain must deploy both history tables"
    );
    assert_eq!(
        columns,
        [
            ("backfill_history_counts", "channel_id", "text"),
            ("backfill_history_counts", "message_count", "bigint"),
            (
                "backfill_history_counts",
                "updated_at",
                "timestamp with time zone"
            ),
            ("backfill_history_messages", "author_id", "text"),
            ("backfill_history_messages", "channel_id", "text"),
            ("backfill_history_messages", "message_id", "text"),
            (
                "backfill_history_messages",
                "sent_at",
                "timestamp with time zone"
            ),
        ]
        .map(|(table, column, kind)| (
            table.to_owned(),
            column.to_owned(),
            kind.to_owned()
        )),
        "deployed history storage must have only the agreed metadata columns"
    );
    assert_eq!(
        primary_keys,
        [
            (
                "backfill_history_counts".to_owned(),
                "channel_id".to_owned()
            ),
            (
                "backfill_history_messages".to_owned(),
                "message_id".to_owned()
            ),
        ],
        "deployment must preserve the keys that make replay idempotent"
    );
    assert_eq!(
        history_roles,
        [
            ("public", "backfill_history_counts", "table"),
            ("public", "backfill_history_messages", "table"),
        ]
        .map(|(schema, name, kind)| (schema.to_owned(), name.to_owned(), kind.to_owned())),
        "the reviewed runtime role matrix must include the deployed history tables"
    );
    assert!(
        erasure_gaps.is_empty(),
        "deployed schema has unclassified identity columns: {erasure_gaps:?}"
    );
}
