#![cfg(test)]
//! Builder-owned live settings and no-effect regression tests.

use crate::hotpath_settings_test_support as hotpath_scope;

#[tokio::test]
async fn raid_runtime_hotpath_settings_reads_are_guild_scoped() {
    use hotpath_scope::queries::Queries;
    let key = "TWO_RAID_JOIN_THRESHOLD";
    let db = hotpath_scope::database(key).await;
    let mut source =
        StoreSettings::new(db.pool().clone(), 22, vars(&[(key, "7")])).with_max_age(Duration::ZERO);
    let (cold, reads) = Queries::capture(source.try_current()).await;
    assert_eq!(cold.unwrap().tuning.threshold(), 3.0);
    hotpath_scope::assert_reads(&reads, Some(2), true);
    let (warm, reads) = Queries::capture(source.try_current()).await;
    assert_eq!(warm.unwrap().tuning.threshold(), 3.0);
    hotpath_scope::assert_reads(&reads, None, true);
    let store = SettingsStore::new(db.pool());
    store
        .set("22", key, Some(serde_json::json!(5)), "fixture")
        .await
        .unwrap();
    let (changed, reads) = Queries::capture(source.try_current()).await;
    assert_eq!(changed.unwrap().tuning.threshold(), 5.0);
    hotpath_scope::assert_reads(&reads, Some(2), true);
    store.set("22", key, None, "fixture").await.unwrap();
    let (fallback, reads) = Queries::capture(source.try_current()).await;
    assert_eq!(fallback.unwrap().tuning.threshold(), 7.0);
    hotpath_scope::assert_reads(&reads, Some(1), true);
    db.close().await.unwrap();
}

use super::*;
use crate::tuning_test_support as support;
use support::{closed_pool, snapshot, vars, Capture, GUILD, SENTINEL};
use tracing::instrument::WithSubscriber;
use tracing::Level;

#[tokio::test]
async fn hot_refusal_is_sticky_and_logs_only_key_transitions() {
    if support::isolated_log_test() {
        return;
    }
    let logs = Capture::default();
    async {
        let mut source = StoreSettings::new(
            closed_pool().await,
            GUILD,
            vars(&[(THRESHOLD_KEY, "8"), (WINDOW_KEY, "30")]),
        );
        let hot = source
            .apply_refresh(Ok(Some(snapshot(&[(THRESHOLD_KEY, "2")]))))
            .expect("hot tuning");
        assert_eq!(hot.tuning.threshold(), 2.0);
        assert_eq!(hot.tuning.window_seconds(), 30.0);
        for value in [SENTINEL, "another-private-invalid-value"] {
            let error = source
                .apply_refresh(Ok(Some(snapshot(&[(THRESHOLD_KEY, value)]))))
                .expect_err("bad hot row must beat valid deployment");
            assert_eq!(error.key(), THRESHOLD_KEY);
        }
        assert_eq!(
            source
                .apply_refresh(Err(()))
                .expect_err("failed read retains refusal")
                .key(),
            THRESHOLD_KEY
        );
        assert_eq!(
            source
                .apply_refresh(Ok(None))
                .expect_err("unchanged marks retain refusal")
                .key(),
            THRESHOLD_KEY
        );
        assert_eq!(
            source
                .apply_refresh(Ok(Some(snapshot(&[(WINDOW_KEY, SENTINEL)]))))
                .expect_err("different refused key")
                .key(),
            WINDOW_KEY
        );
        let recovered = source
            .apply_refresh(Ok(Some(snapshot(&[
                (THRESHOLD_KEY, "3"),
                (WINDOW_KEY, "15"),
            ]))))
            .expect("valid snapshot recovers");
        assert_eq!(recovered.tuning.threshold(), 3.0);
        assert_eq!(source.apply_refresh(Ok(None)), Ok(recovered));
        assert!(source
            .apply_refresh(Ok(Some(snapshot(&[(THRESHOLD_KEY, SENTINEL)]))))
            .is_err());
        let deleted = source
            .apply_refresh(Ok(Some(snapshot(&[]))))
            .expect("deleted row returns to deployment");
        assert_eq!(deleted.tuning.threshold(), 8.0);
        assert_eq!(source.apply_refresh(Err(())), Ok(deleted));
    }
    .with_subscriber(logs.subscriber())
    .await;
    let records = logs.records();
    let errors: Vec<_> = records
        .iter()
        .filter(|record| record.level == Level::ERROR)
        .collect();
    assert_eq!(
        errors.len(),
        3,
        "first refusal, changed key, refusal after recovery"
    );
    assert_eq!(
        errors
            .iter()
            .map(|record| record.fields["key"].as_str())
            .collect::<Vec<_>>(),
        [THRESHOLD_KEY, WINDOW_KEY, THRESHOLD_KEY]
    );
    assert!(errors
        .iter()
        .all(|record| record.fields["message"].contains("raid_tuning_refused")));
    let text = format!("{records:?}");
    assert!(!text.contains(SENTINEL));
    assert!(!text.contains("another-private-invalid-value"));
}

#[tokio::test]
async fn refused_worker_does_not_score_post_or_consume_cooldown() {
    if support::isolated_log_test() {
        return;
    }
    let logs = Capture::default();
    async {
        for key in [WINDOW_KEY, THRESHOLD_KEY, CHANNEL_KEY] {
            let (executor, trap) = support::no_discord();
            let mut deployment = vars(&[(THRESHOLD_KEY, "2"), (CHANNEL_KEY, "223456789012345678")]);
            deployment.insert(key.to_owned(), SENTINEL.to_owned());
            let (_, joins) = mpsc::channel(1);
            let mut worker = RaidWorker {
                guild_id: GUILD,
                settings: StoreSettings::new(closed_pool().await, GUILD, deployment),
                delivery: RaidDelivery::new(executor, GUILD),
                watch: RaidWatch::default(),
                joins,
                shutdown: Arc::new(WorkerShutdown::default()),
            };
            for n in 0..6 {
                tokio::time::timeout(
                    Duration::from_secs(2),
                    worker.handle(JoinObservation {
                        guild_id: GUILD,
                        member_id: 1000 + n,
                        source: "unknown".into(),
                        joined_at_ms: 1_790_780_400_000 + n as i64,
                    }),
                )
                .await
                .expect("refusal must not wait on delivery");
            }
            support::assert_no_discord(&trap);
            // If refused joins entered the watch, the first probe alerts, or
            // a consumed cooldown suppresses the second. Neither is allowed.
            let tuning = RaidTuning::new(60.0, 2.0).expect("valid probe");
            assert!(worker
                .watch
                .observe(&GUILD.to_string(), "2000", 1_790_780_400_010, tuning)
                .is_none());
            let alert = worker
                .watch
                .observe(&GUILD.to_string(), "2001", 1_790_780_400_011, tuning)
                .expect("fresh valid burst still alerts");
            assert_eq!(alert.count, 2);
            assert_eq!(alert.member_ids, ["2000", "2001"]);
        }
    }
    .with_subscriber(logs.subscriber())
    .await;
    let records = logs.records();
    let errors: Vec<_> = records
        .iter()
        .filter(|record| record.level == Level::ERROR)
        .collect();
    assert_eq!(errors.len(), 3, "one refusal per source, not per join");
    assert!(errors
        .iter()
        .all(|record| record.fields["message"].contains("raid_tuning_refused")));
    assert!(!format!("{records:?}").contains(SENTINEL));
}

#[tokio::test]
async fn hot_valid_row_masks_bad_deployment_until_deleted() {
    let mut source = StoreSettings::new(
        closed_pool().await,
        GUILD,
        vars(&[(THRESHOLD_KEY, SENTINEL)]),
    );
    assert_eq!(
        source
            .apply_refresh(Ok(Some(snapshot(&[(THRESHOLD_KEY, "4")]))))
            .expect("hot wins")
            .tuning
            .threshold(),
        4.0
    );
    assert_eq!(
        source
            .apply_refresh(Ok(Some(snapshot(&[]))))
            .expect_err("deletion exposes malformed deployment")
            .key(),
        THRESHOLD_KEY
    );
    assert_eq!(
        source
            .apply_refresh(Err(()))
            .expect_err("no recovery on failed read")
            .key(),
        THRESHOLD_KEY
    );
    assert!(source
        .apply_refresh(Ok(Some(snapshot(&[(THRESHOLD_KEY, "4")]))))
        .is_ok());
}

#[tokio::test]
async fn current_caches_good_and_refused_outcomes_and_failure_preserves_both() {
    if support::isolated_log_test() {
        return;
    }
    for value in ["7", SENTINEL] {
        let logs = Capture::default();
        async {
            let mut source =
                StoreSettings::new(closed_pool().await, GUILD, vars(&[(THRESHOLD_KEY, value)]));
            let first = source.try_current().await;
            assert_eq!(first.is_ok(), value == "7");
            for _ in 0..3 {
                assert_eq!(source.try_current().await, first);
            }
            assert_eq!(
                logs.records()
                    .iter()
                    .filter(|record| record.level == Level::WARN)
                    .count(),
                1,
                "cached reads do not refresh"
            );
            source.checked = None;
            assert_eq!(
                source.try_current().await,
                first,
                "failed refresh retains the last outcome"
            );
            assert_eq!(
                logs.records()
                    .iter()
                    .filter(|record| record.level == Level::WARN)
                    .count(),
                2
            );
            assert_eq!(
                logs.records()
                    .iter()
                    .filter(|record| record.level == Level::ERROR)
                    .count(),
                usize::from(value == SENTINEL)
            );
        }
        .with_subscriber(logs.subscriber())
        .await;
    }
}
