use super::*;
use two_bot_testsupport::TestDatabase;

const WARNING_HISTORY_INDEX_SQL: &str =
    include_str!("../../../../cutover/migration-candidates/moderation_warning_history_order.sql");

#[tokio::test]
async fn tie_heavy_history_uses_bounded_index_ranges() {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert_ne!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
        return;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap();
    sqlx::raw_sql(
        "INSERT INTO moderation_warnings
             (id, guild_id, user_id, actor_id, reason, request_id, created_at)
         SELECT lpad(n::text, 5, '0'), CASE WHEN n <= 12000 THEN '1' ELSE '2' END,
                '3', '4', 'synthetic', 'r-' || n, '2026-10-01 00:00:00+00'
         FROM generate_series(1, 24000) AS n;
         ANALYZE moderation_warnings;",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::raw_sql(WARNING_HISTORY_INDEX_SQL)
        .execute(db.pool())
        .await
        .unwrap();
    let created_at = OffsetDateTime::parse("2026-10-01T00:00:00Z", &Rfc3339).unwrap();
    let cursor = WarningHistoryCursor {
        guild: "1".into(),
        target: "3".into(),
        high_water: Key {
            created_at,
            id: "12000".into(),
        },
        last_shown: Key {
            created_at,
            id: "00600".into(),
        },
    };
    for cursor in [None, Some(&cursor)] {
        let mut query = QueryBuilder::new("EXPLAIN (ANALYZE, FORMAT JSON) ");
        push_history_query(&mut query, "1", "3", cursor);
        let plan = query
            .build_query_scalar::<serde_json::Value>()
            .fetch_one(db.pool())
            .await
            .unwrap();
        let mut limit = &plan[0]["Plan"];
        while limit["Node Type"] != "Limit" {
            let children = limit["Plans"].as_array().expect("ordered limit");
            assert_eq!(children.len(), 1, "{plan}");
            assert_ne!(limit["Node Type"], "Sort", "{plan}");
            limit = &children[0];
        }
        let scan = &limit["Plans"][0];
        assert_eq!(
            scan["Index Name"], "idx_moderation_warnings_history_order",
            "{plan}"
        );
        assert_eq!(scan["Actual Rows"].as_f64(), Some(6.0), "{plan}");
        assert!(scan.get("Filter").is_none(), "{plan}");
        if cursor.is_some() {
            assert!(
                scan["Index Cond"].as_str().unwrap().contains("ROW("),
                "{plan}"
            );
        }
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn warning_index_rejects_oversize_combined_historical_key() {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => {
            assert_ne!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
            return;
        }
        Err(_) => panic!("TWO_TEST_DATABASE_URL must be Unicode"),
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated disposable fixture");
    // Spread the bytes across the three text keys. Use deterministic hash
    // material so TOAST compression does not hide the size boundary.
    let insert = "INSERT INTO moderation_warnings
                      (id, guild_id, user_id, actor_id, reason, request_id, created_at)
                  SELECT substr(bytes, 1801, $1), substr(bytes, 1, 900),
                         substr(bytes, 901, 900), '4', 'synthetic',
                         'synthetic-key-' || $1::int::text, '2026-10-01 00:00:00+00'
                  FROM (SELECT string_agg(md5(n::text), '' ORDER BY n) AS bytes
                        FROM generate_series(1, 84) AS n) AS fixture";

    // With the 8 KiB pages used in CI, these lengths put the index tuple at
    // 2704 bytes (including header, varlena headers and timestamp), then one
    // payload byte past it. The existing indexes admit both rows.
    let outcome = async {
        sqlx::query(insert).bind(876).execute(db.pool()).await?;
        sqlx::raw_sql(WARNING_HISTORY_INDEX_SQL)
            .execute(db.pool())
            .await?;
        sqlx::query("DROP INDEX idx_moderation_warnings_history_order")
            .execute(db.pool())
            .await?;
        sqlx::query(insert).bind(877).execute(db.pool()).await?;
        let build = sqlx::raw_sql(WARNING_HISTORY_INDEX_SQL)
            .execute(db.pool())
            .await;
        let index_valid: bool = sqlx::query_scalar(
            "SELECT indisvalid FROM pg_index
             WHERE indexrelid = 'idx_moderation_warnings_history_order'::regclass
               AND indrelid = 'moderation_warnings'::regclass",
        )
        .fetch_one(db.pool())
        .await?;
        let key_sizes: Vec<i32> = sqlx::query_scalar(
            "SELECT octet_length(guild_id) + octet_length(user_id) + octet_length(id)
             FROM moderation_warnings ORDER BY 1",
        )
        .fetch_all(db.pool())
        .await?;
        Ok::<_, sqlx::Error>((build, index_valid, key_sizes))
    }
    .await;
    db.close().await.expect("drop disposable fixture");

    let (build, index_valid, key_sizes) = outcome.expect("exercise candidate index boundary");
    let error = build.expect_err("oversize historical key must fail the candidate build");
    let error = error.as_database_error().expect("PostgreSQL build error");
    assert_eq!(error.code().as_deref(), Some("54000"));
    assert!(error.message().contains("index row size"));
    assert!(!index_valid);
    assert_eq!(key_sizes, [2676, 2677]);
}
