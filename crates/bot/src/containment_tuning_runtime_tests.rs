#![cfg(test)]
//! Builder-owned live settings, boot refusal and no-effect regression tests.

use crate::hotpath_settings_test_support as hotpath_scope;

// Independently catches a global snapshot on cold refresh and a global poll
// count on warm refresh, through the actual runtime (not a store-only call).
#[tokio::test]
async fn containment_worker_hotpath_settings_reads_are_guild_scoped() {
    use hotpath_scope::queries::Queries;
    let key = "TWO_ANTI_NUKE_HEAT_THRESHOLD";
    let db = hotpath_scope::database(key).await;
    let mut source = StoreSettings::new(db.pool().clone(), 22, vars(&[(key, "7")]));
    source.max_age = Duration::ZERO;
    let (cold, reads) = Queries::capture(source.try_current()).await;
    assert_eq!(cold.unwrap().heat_threshold, 3);
    hotpath_scope::assert_reads(&reads, Some(2), true);
    let (warm, reads) = Queries::capture(source.try_current()).await;
    assert_eq!(warm.unwrap().heat_threshold, 3);
    hotpath_scope::assert_reads(&reads, None, true);
    let store = SettingsStore::new(db.pool());
    store
        .set("22", key, Some(serde_json::json!(5)), "fixture")
        .await
        .unwrap();
    let (changed, reads) = Queries::capture(source.try_current()).await;
    assert_eq!(changed.unwrap().heat_threshold, 5);
    hotpath_scope::assert_reads(&reads, Some(2), true);
    store.set("22", key, None, "fixture").await.unwrap();
    let (fallback, reads) = Queries::capture(source.try_current()).await;
    assert_eq!(fallback.unwrap().heat_threshold, 7);
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
            vars(&[(HEAT_KEY, "8"), (WINDOW_KEY, "30")]),
        );
        let hot = source
            .apply_refresh(Ok(Some(snapshot(&[(HEAT_KEY, "2")]))))
            .expect("hot tuning");
        assert_eq!(hot.heat_threshold, 2);
        assert_eq!(hot.window_ms, 30_000);
        for value in [SENTINEL, "another-private-invalid-value"] {
            assert_eq!(
                source
                    .apply_refresh(Ok(Some(snapshot(&[(HEAT_KEY, value)]))))
                    .expect_err("bad hot row must beat valid deployment")
                    .key(),
                HEAT_KEY
            );
        }
        assert_eq!(
            source
                .apply_refresh(Err(()))
                .expect_err("failed read retains refusal")
                .key(),
            HEAT_KEY
        );
        assert_eq!(
            source
                .apply_refresh(Ok(None))
                .expect_err("unchanged marks retain refusal")
                .key(),
            HEAT_KEY
        );
        assert_eq!(
            source
                .apply_refresh(Ok(Some(snapshot(&[(MAX_AGE_KEY, SENTINEL)]))))
                .expect_err("different refused key")
                .key(),
            MAX_AGE_KEY
        );
        let recovered = source
            .apply_refresh(Ok(Some(snapshot(&[(HEAT_KEY, "3"), (WINDOW_KEY, "15")]))))
            .expect("valid snapshot recovers");
        assert_eq!(recovered.heat_threshold, 3);
        assert_eq!(source.apply_refresh(Ok(None)), Ok(recovered));
        assert!(source
            .apply_refresh(Ok(Some(snapshot(&[(HEAT_KEY, SENTINEL)]))))
            .is_err());
        let deleted = source
            .apply_refresh(Ok(Some(snapshot(&[]))))
            .expect("deletion restores deployment");
        assert_eq!(deleted.heat_threshold, 8);
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
        [HEAT_KEY, MAX_AGE_KEY, HEAT_KEY]
    );
    assert!(errors
        .iter()
        .all(|record| record.fields["message"].contains("containment_tuning_refused")));
    let text = format!("{records:?}");
    assert!(!text.contains(SENTINEL));
    assert!(!text.contains("another-private-invalid-value"));
}

#[tokio::test]
async fn hot_valid_row_masks_bad_deployment_until_deleted() {
    let mut source = StoreSettings::new(closed_pool().await, GUILD, vars(&[(HEAT_KEY, SENTINEL)]));
    assert_eq!(
        source
            .apply_refresh(Ok(Some(snapshot(&[(HEAT_KEY, "4")]))))
            .expect("hot wins")
            .heat_threshold,
        4
    );
    assert_eq!(
        source
            .apply_refresh(Ok(Some(snapshot(&[]))))
            .expect_err("deletion exposes malformed deployment")
            .key(),
        HEAT_KEY
    );
    assert_eq!(
        source
            .apply_refresh(Err(()))
            .expect_err("no recovery on failed read")
            .key(),
        HEAT_KEY
    );
    assert!(source
        .apply_refresh(Ok(Some(snapshot(&[(HEAT_KEY, "4")]))))
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
                StoreSettings::new(closed_pool().await, GUILD, vars(&[(HEAT_KEY, value)]));
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

#[tokio::test]
async fn refused_worker_does_not_claim_alert_or_remove_roles() {
    if support::isolated_log_test() {
        return;
    }
    let logs = Capture::default();
    async {
        for key in [WINDOW_KEY, MAX_AGE_KEY, HEAT_KEY, CHANNEL_KEY] {
            for armed in [false, true] {
                let (executor, trap) = support::no_discord();
                let pool = closed_pool().await;
                let mut deployment = vars(&[(HEAT_KEY, "1"), (CHANNEL_KEY, "223456789012345678")]);
                deployment.insert(key.to_owned(), SENTINEL.to_owned());
                let (_, entries) = mpsc::channel(1);
                let mut worker = ContainmentWorker {
                    guild_id: GUILD,
                    settings: StoreSettings::new(pool.clone(), GUILD, deployment),
                    store: ContainmentStore::from_pool(pool),
                    delivery: RaidDelivery::new(executor.clone(), GUILD),
                    executor,
                    trusted_user_ids: HashSet::new(),
                    protected_user_ids: HashSet::new(),
                    bot_user_id: "999".into(),
                    armed,
                    entries,
                    shutdown: Arc::new(crate::raid_runtime::WorkerShutdown::default()),
                };
                let entry = AuditLogObservation {
                    guild_id: GUILD,
                    entry_id:
                        two_bot_cutover::parse::date_to_snowflake(processing_now_ms(0) as u64)
                            .parse()
                            .expect("fresh entry snowflake"),
                    action: AuditLogEventType::ChannelDelete,
                    executor_id: Some(5001),
                    target_id: Some(6001),
                };
                let start = logs.records().len();
                for _ in 0..3 {
                    tokio::time::timeout(Duration::from_secs(2), worker.handle(entry.clone()))
                        .await
                        .expect("refusal must not wait on effects");
                }
                let before_recovery = logs.records();
                let errors: Vec<_> = before_recovery[start..]
                    .iter()
                    .filter(|record| record.level == Level::ERROR)
                    .collect();
                assert_eq!(
                    errors.len(),
                    1,
                    "no claim attempt, policy failure, alert, or log flood"
                );
                assert_eq!(errors[0].fields["key"], key);
                assert!(errors[0].fields["message"].contains("containment_tuning_refused"));
                support::assert_no_discord(&trap);
                // Positive control: the same entry reaches claim_event after
                // a valid read. The closed pool reports that attempt, without
                // ever connecting to a database or recording a real claim.
                worker.settings.deployment.clear();
                assert!(worker
                    .settings
                    .apply_refresh(Ok(Some(snapshot(&[]))))
                    .is_ok());
                worker.handle(entry).await;
                let after = logs.records();
                assert_eq!(
                    after[start..]
                        .iter()
                        .filter(|record| record
                            .fields
                            .get("message")
                            .is_some_and(|message| message.contains("containment_store_failed")))
                        .count(),
                    1,
                    "valid settings reach the durable-claim boundary"
                );
                support::assert_no_discord(&trap);
            }
        }
    }
    .with_subscriber(logs.subscriber())
    .await;
    assert!(!format!("{:?}", logs.records()).contains(SENTINEL));
}

#[tokio::test]
async fn malformed_cold_lists_refuse_boot_before_identity_or_effects() {
    if support::isolated_log_test() {
        return;
    }
    let logs = Capture::default();
    async {
        let guild = two_bot_cutover::STAGING_GUILD_ID
            .parse()
            .expect("staging fence ID");
        for key in [TRUSTED_KEY, PROTECTED_KEY] {
            let (executor, trap) = support::no_discord();
            let mixed = format!("123456789012345678,{SENTINEL},223456789012345678");
            let input = vars(&[
                ("TWO_ANTI_NUKE", "1"),
                ("TWO_ANTI_NUKE_DRY_RUN", "0"),
                (key, &mixed),
            ]);
            assert!(
                AntiNukeFences::from_vars(&input, guild).armed,
                "positive arming control"
            );
            assert!(start_from_vars(
                closed_pool().await,
                executor,
                guild,
                &input,
                tokio::sync::watch::channel(false).1
            )
            .is_none());
            support::assert_no_discord(&trap);
        }
        // Disabled fences stay disabled; malformed lists grant no activation.
        let (executor, trap) = support::no_discord();
        assert!(start_from_vars(
            closed_pool().await,
            executor,
            guild,
            &vars(&[(TRUSTED_KEY, SENTINEL)]),
            tokio::sync::watch::channel(false).1
        )
        .is_none());
        support::assert_no_discord(&trap);
    }
    .with_subscriber(logs.subscriber())
    .await;
    let records = logs.records();
    let errors: Vec<_> = records
        .iter()
        .filter(|record| record.level == Level::ERROR)
        .collect();
    assert_eq!(errors.len(), 2);
    assert_eq!(
        errors
            .iter()
            .map(|record| record.fields["key"].as_str())
            .collect::<Vec<_>>(),
        [TRUSTED_KEY, PROTECTED_KEY]
    );
    assert!(errors
        .iter()
        .all(|record| record.fields["message"].contains("containment_tuning_refused")));
    assert!(!format!("{records:?}").contains(SENTINEL));
}

#[tokio::test]
async fn production_vars_constructor_defaults_to_dry_run_and_joins_with_live_observer() {
    if support::isolated_log_test() {
        return;
    }
    let logs = Capture::default();
    async {
        let guild = two_bot_cutover::STAGING_GUILD_ID
            .parse()
            .expect("staging fence ID");
        let (executor, mock) = support::containment_identity().await;
        let (shutdown, stopping) = tokio::sync::watch::channel(false);
        // No explicit dry-run override: the production default must install
        // observation without arming role removals. Only startup identity
        // GETs can run; no events are queued for DB-dependent processing.
        let (observer, mut supervisor) = start_from_vars(
            closed_pool().await,
            executor,
            guild,
            &vars(&[("TWO_ANTI_NUKE", "1")]),
            stopping,
        )
        .expect("enabled default dry-run installs a supervised observer");
        assert!(futures_util::poll!(&mut supervisor).is_pending());
        shutdown.send(true).expect("service shutdown");
        tokio::time::timeout(Duration::from_secs(5), supervisor)
            .await
            .expect("constructor must propagate shutdown without sender-drop")
            .expect("constructor supervisor joins cleanly");
        let weak = Arc::downgrade(&observer);
        drop(observer);
        assert!(
            weak.upgrade().is_none(),
            "joined supervisor releases observer"
        );
        support::assert_only_containment_identity(mock).await;
    }
    .with_subscriber(logs.subscriber())
    .await;
    let fences: Vec<_> = logs
        .records()
        .into_iter()
        .filter(|record| {
            record
                .fields
                .get("message")
                .is_some_and(|message| message.contains("containment_fences"))
        })
        .collect();
    assert_eq!(fences.len(), 1, "constructor reports its actual boot fence");
    assert_eq!(
        fences[0].fields["armed"], "false",
        "default is safe dry-run"
    );
}
