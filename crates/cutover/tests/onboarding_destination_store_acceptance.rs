//! Protected acceptance: validation precedes SQL, CAS, audit and live refresh.
//! Closed-pool tests need no DB; the durable control uses only TestDatabase.

#[path = "../../core/tests/support/onboarding_destination_cases.rs"]
mod cases;

use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use two_bot_core::internal_actions::ErrorCode;
use two_bot_core::internal_settings::SettingsCommand;
use two_bot_core::settings::live_channel;
use two_bot_cutover::internal_settings::execute_settings;
use two_bot_cutover::settings::{SettingsStore, SettingsWriteError};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "111111111111111111";
const ACTOR: &str = "222222222222222222";
const EDITOR: &str = "333333333333333333";
type TestResult = Result<(), Box<dyn std::error::Error>>;

fn command(key: &str, value: Value, actor: &str, version: i64) -> SettingsCommand {
    SettingsCommand::parse(
        "settings.set",
        json!({
            "key": key, "value": value, "updated_by": actor, "expected_version": version
        })
        .as_object()
        .unwrap(),
    )
    .unwrap()
}

async fn closed_pool() -> sqlx::PgPool {
    // No connection is made: closing the lazy pool makes *any* SQL fail.
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_local")
        .unwrap();
    pool.close().await;
    pool
}

#[tokio::test]
async fn invalid_destinations_refuse_before_pool_acquisition_or_cas() {
    let pool = closed_pool().await;
    let store = SettingsStore::new(&pool);
    for key in cases::ALL {
        for value in cases::invalid(key) {
            // An intentionally stale CAS token must not mask syntax refusal.
            let result = store
                .set_if_version(GUILD, key, Some(value), ACTOR, Some(-99))
                .await;
            assert!(
                matches!(result, Err(SettingsWriteError::Refused(_))),
                "{key}: invalid value attempted SQL instead of typed validation"
            );
        }
    }
}

#[tokio::test]
async fn execution_maps_invalid_destination_to_safe_nonretryable_repairable_refusal() {
    let pool = closed_pool().await;
    let store = SettingsStore::new(&pool);
    for key in cases::ALL {
        for value in [
            json!("private-destination-sentinel"),
            json!({"private-destination-sentinel": 42}),
        ] {
            let error = execute_settings(&store, GUILD, &command(key, value, ACTOR, -99))
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::Malformed);
            assert_eq!(error.status(), 400);
            assert!(!error.code.retryable());
            assert_eq!(error.log_reason, "settings_value_invalid");
            assert!(error.message.contains(key));
            assert!(error.message.contains("delete"));
            assert!(error.message.contains("fallback") || error.message.contains("fall back"));
            for output in [error.message.to_string(), format!("{error:?}")] {
                for private in [
                    "private-destination-sentinel",
                    GUILD,
                    ACTOR,
                    "postgres",
                    "SELECT",
                ] {
                    assert!(
                        !output.contains(private),
                        "error leaked supplied/store data"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn valid_and_delete_closed_pool_controls_reach_storage() {
    let pool = closed_pool().await;
    let store = SettingsStore::new(&pool);
    for key in cases::ALL {
        // Use all valid fixtures here too, so both sides of the shared corpus
        // remain exercised in this standalone integration-test target.
        for (value, _, _) in cases::valid(key) {
            assert!(matches!(
                store.set(GUILD, key, Some(value), ACTOR).await,
                Err(SettingsWriteError::Db(sqlx::Error::PoolClosed))
            ));
        }
        assert!(matches!(
            store.set(GUILD, key, None, ACTOR).await,
            Err(SettingsWriteError::Db(sqlx::Error::PoolClosed))
        ));
    }
}

#[tokio::test]
async fn durable_row_cas_audit_and_live_snapshot_survive_invalid_writes() -> TestResult {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => {
            eprintln!("SKIP durable destination test: TWO_TEST_DATABASE_URL not set");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("./migrations")).await?;
    let store = SettingsStore::new(db.pool());
    let (mut writer, live) = live_channel();
    let mut results = Vec::new();
    for key in cases::ALL {
        let seed = execute_settings(&store, GUILD, &command(key, json!(42), ACTOR, 0)).await?;
        let token = seed.observed_version;
        assert!(token < 0);
        writer.publish(&store.load_snapshot().await?);
        let before_live = live.snapshot();
        let before = store.poll_marks().await?;
        let row: (Value, i64, i64, String, String) = sqlx::query_as(
            "SELECT value, version, cas_version, updated_by, updated_at::text FROM guild_settings WHERE guild_id=$1 AND key=$2"
        ).bind(GUILD).bind(key).fetch_one(db.pool()).await?;
        let audit: i64 = sqlx::query_scalar("SELECT count(*) FROM guild_settings_audit")
            .fetch_one(db.pool())
            .await?;

        // Capture observations first and close the disposable fixture before
        // asserting RED; failed acceptance must not strand a database.
        let mut observations = Vec::new();
        for invalid in [
            json!("42,private-destination-sentinel"),
            json!(0),
            json!({"bad": 42}),
        ] {
            let refused =
                execute_settings(&store, GUILD, &command(key, invalid, EDITOR, token)).await;
            let current: (Value, i64, i64, String, String) = sqlx::query_as(
                "SELECT value, version, cas_version, updated_by, updated_at::text FROM guild_settings WHERE guild_id=$1 AND key=$2"
            ).bind(GUILD).bind(key).fetch_one(db.pool()).await?;
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM guild_settings_audit")
                .fetch_one(db.pool())
                .await?;
            let marks = store.poll_marks().await?;
            let report = writer.publish(&store.load_snapshot().await?);
            observations.push((
                refused.is_err_and(|e| e.log_reason == "settings_value_invalid"),
                current == row,
                count == audit,
                marks == before,
                !report.changed,
                Arc::ptr_eq(&before_live, &live.snapshot()),
            ));
        }
        // Restore a valid value using the current token even on the unfixed
        // baseline, allowing the remaining attribution/CAS controls to run.
        let current = store.get(GUILD, key).await?.unwrap().1;
        let update =
            execute_settings(&store, GUILD, &command(key, json!(43), EDITOR, current)).await?;
        let update_actor: String = sqlx::query_scalar(
            "SELECT updated_by FROM guild_settings WHERE guild_id=$1 AND key=$2",
        )
        .bind(GUILD)
        .bind(key)
        .fetch_one(db.pool())
        .await?;
        let stale = execute_settings(&store, GUILD, &command(key, json!(44), ACTOR, token))
            .await
            .unwrap_err();
        assert_eq!(stale.code, ErrorCode::VersionConflict);
        let deleted = execute_settings(
            &store,
            GUILD,
            &command(key, Value::Null, ACTOR, update.observed_version),
        )
        .await?;
        writer.publish(&store.load_snapshot().await?);
        let absent = store.get(GUILD, key).await?.is_none();
        let fallback = !live.env_snapshot(Some(GUILD)).contains_key(key);
        let last_two: Vec<(Option<Value>, Option<Value>, String)> = sqlx::query_as(
            "SELECT old_value, new_value, actor FROM guild_settings_audit WHERE guild_id=$1 AND key=$2 ORDER BY id DESC LIMIT 2"
        ).bind(GUILD).bind(key).fetch_all(db.pool()).await?;
        assert_eq!(update_actor, EDITOR);
        assert_ne!(update.observed_version, current);
        assert_eq!(deleted.observed_version, 0);
        assert!(absent && fallback);
        assert_eq!(last_two[0], (Some(json!(43)), None, ACTOR.into()));
        assert_eq!(last_two[1].1, Some(json!(43)));
        assert_eq!(last_two[1].2, EDITOR);
        // Do not stop at the first key before fixture teardown.
        results.extend(observations.into_iter().map(|observed| (key, observed)));
    }
    db.close().await?;
    for (key, observed) in results {
        assert_eq!(
            observed,
            (true, true, true, true, true, true),
            "{key}: refusal must preserve row/CAS/timestamp/actor, audit, marks and live Arc"
        );
    }
    Ok(())
}
