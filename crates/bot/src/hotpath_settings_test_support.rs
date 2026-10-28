#![cfg(test)]
//! Runtime-boundary query observations. A global snapshot returns 32 rows,
//! not 2; a global poll count forces an otherwise unnecessary warm reload.

#[path = "../../core/tests/support/hotpath_queries.rs"]
pub mod queries;

use queries::Query;
use serde_json::json;
use two_bot_cutover::settings::SettingsStore;
use two_bot_testsupport::TestDatabase;

pub async fn database(key: &str) -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("runtime scope regression requires disposable TWO_TEST_DATABASE_URL");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap();
    let store = SettingsStore::new(db.pool());
    let (foreign_value, target_value) = if key == "TWO_ONBOARDING_DRY_RUN" {
        (json!("1"), json!("0"))
    } else {
        (json!(9), json!(3))
    };
    for index in 0..30 {
        store
            .set(
                &format!("{}", 300 + index),
                key,
                Some(foreign_value.clone()),
                "fixture",
            )
            .await
            .unwrap();
    }
    store
        .set("22", key, Some(target_value), "fixture")
        .await
        .unwrap();
    store
        .set(
            "22",
            "DISCORD_STAFF_ALERT_CHANNEL_ID",
            Some(json!("123456789012345678")),
            "fixture",
        )
        .await
        .unwrap();
    db
}

pub fn assert_reads(receipts: &[Query], snapshot_rows: Option<u64>, poll: bool) {
    let snapshots: Vec<_> = receipts
        .iter()
        .filter(|q| {
            let q = q.normalized();
            q.contains("from guild_settings") && q.contains("key") && q.contains("value")
        })
        .collect();
    match snapshot_rows {
        Some(want) => {
            assert!(!snapshots.is_empty(), "actual runtime never loaded settings (observer or wiring broken)");
            for query in snapshots {
                assert_eq!(query.rows, want, "runtime materialized foreign guild rows: {}", query.sql);
            }
        }
        None => assert!(snapshots.is_empty(), "unchanged scoped marks must not reload a snapshot; a global poll count disagrees with the local cache"),
    }
    let polls: Vec<_> = receipts
        .iter()
        .filter(|q| {
            let q = q.normalized();
            q.contains("guild_settings_revision") && q.contains("count(*)")
        })
        .collect();
    if poll {
        assert_eq!(
            polls.len(),
            1,
            "actual runtime must poll exactly once, not short-circuit from a fresh cache"
        );
        assert_eq!(polls[0].rows, 1);
    } else {
        assert!(polls.is_empty());
    }
}
