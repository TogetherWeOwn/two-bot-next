use super::*;
use two_bot_testsupport::TestDatabase;

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
        assert_eq!(scan["Actual Rows"], 6, "{plan}");
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
