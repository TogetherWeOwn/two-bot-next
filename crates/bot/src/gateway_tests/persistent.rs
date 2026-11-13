//! S5/S6 integration: all effects, including invite baselines and bot flags,
//! must roll back with a failed checkpoint. Only test-container schemas.
use super::*;
use futures_util::FutureExt as _;
use two_bot_core::gateway_funnel::{ActivityTouch, SnapshotWrite};
use two_bot_core::{analytics_family::AnalyticsCategory, InviteSnapshotStore, InviteState};
use two_bot_discord::MessageEligibility;

fn invite(uses: u64) -> InviteState {
    InviteState {
        code: "fixture".into(),
        uses,
        inviter_id: Some(9),
        channel_id: Some(8),
        max_uses: 0,
        temporary: false,
    }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn snapshot_bot_activity_and_checkpoint_commit_or_roll_back_together() {
    let db = TestDb::new().await;
    let result =
        std::panic::AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(25), async {
            snapshot_case(&db).await;
            writer_cases(&db, &db.store, RESUME_URL, RESUME_URL).await;
            writer_policies(&db).await;
        }))
        .catch_unwind()
        .await;
    db.close().await;
    result
        .expect("transaction assertions")
        .expect("transaction deadline");
}

async fn snapshot_case(db: &TestDb) {
    two_bot_store::migrate(&db.pool).await.unwrap();
    two_bot_store::apply_web_contract(&db.pool).await.unwrap();
    db.store
        .commit_dispatch(
            &checkpoint("atomic", 1, RESUME_URL),
            FunnelBatch {
                snapshots: vec![SnapshotWrite::StoreAll(2222, vec![invite(1)])],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let bad = FunnelBatch {
        bots: vec![(2222, 77)],
        snapshots: vec![SnapshotWrite::StoreAll(2222, vec![invite(2)])],
        activity: vec![ActivityTouch {
            guild_id: 2222,
            member_id: 88,
            at: "2026-09-30T04:00:00.000Z".into(),
            family: AnalyticsCategory::Activity,
        }],
        events: vec![event(EventType::MemberLeave, "not-a-timestamp")],
        ..Default::default()
    };
    assert!(db
        .store
        .commit_dispatch(&checkpoint("atomic", 2, RESUME_URL), bad)
        .await
        .is_err());
    assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 1);
    assert_eq!(db.store.invite_snapshots().await.unwrap()[0].uses, 1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM members")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "bot/recency must not escape the failed transaction"
    );
    assert_eq!(db.count().await, 0);

    db.store
        .commit_dispatch(
            &checkpoint("atomic", 2, RESUME_URL),
            FunnelBatch {
                bots: vec![(2222, 77)],
                snapshots: vec![SnapshotWrite::StoreAll(2222, vec![invite(2)])],
                activity: vec![ActivityTouch {
                    guild_id: 2222,
                    member_id: 88,
                    at: "2026-09-30T04:00:00.000Z".into(),
                    family: AnalyticsCategory::Activity,
                }],
                events: vec![event(EventType::MemberLeave, "2026-09-30T04:00:01.000Z")],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 2);
    assert_eq!(db.store.invite_snapshots().await.unwrap()[0].uses, 2);
    let bot: (bool, bool) =
        sqlx::query_as("SELECT is_bot, left_at IS NOT NULL FROM members WHERE member_id = '77'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(bot, (true, true));
    let active: bool =
        sqlx::query_scalar("SELECT last_active_at IS NOT NULL FROM members WHERE member_id = '88'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(active, "activity-only dispatch must create its member row");
    let view = sqlx::AssertSqlSafe(format!(
        "SELECT COALESCE(sum(leaves), 0)::bigint FROM {}_web_v1.funnel_daily",
        db.schema
    ));
    let visible: i64 = sqlx::query_scalar(view).fetch_one(&db.pool).await.unwrap();
    assert_eq!(visible, 0, "bot departure excluded from the human contract");
    // Duplicate sequence may not mutate the invite baseline or classification.
    assert_eq!(
        db.store
            .commit_dispatch(
                &checkpoint("atomic", 2, RESUME_URL),
                FunnelBatch {
                    snapshots: vec![SnapshotWrite::StoreAll(2222, vec![invite(99)])],
                    ..Default::default()
                }
            )
            .await
            .unwrap(),
        DispatchAction::Duplicate
    );
    assert_eq!(db.store.invite_snapshots().await.unwrap()[0].uses, 2);
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn persistent_pipeline_hydrates_invites_and_stages_bot_departure_on_restart() {
    let db = TestDb::new().await;
    db.store
        .commit_dispatch(
            &checkpoint("restart", 1, RESUME_URL),
            FunnelBatch {
                snapshots: vec![SnapshotWrite::StoreAll(2222, vec![invite(7)])],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let pipeline = crate::gateway::build_persistent_pipeline(
        &db.store,
        2222,
        TOKEN.into(),
        None,
        crate::voice_snapshot::new_shared_cache(),
    )
    .await
    .unwrap();
    assert_eq!(pipeline.handlers().store().load(2222)[0].uses, 7);
    assert!(
        pipeline
            .handlers()
            .store()
            .take_batch()
            .snapshots
            .is_empty(),
        "hydration must not rewrite the baseline"
    );
    let remove = twilight_gateway::Event::MemberRemove(
        twilight_model::gateway::payload::incoming::MemberRemove {
            guild_id: twilight_model::id::Id::new(2222),
            user: serde_json::from_value(
                json!({"id":"77","username":"fixture-bot","discriminator":"0","bot":true}),
            )
            .unwrap(),
        },
    );
    pipeline
        .handle_at(
            &remove,
            "2026-09-30T04:00:01.000Z",
            MessageEligibility::default(),
        )
        .await
        .unwrap();
    let batch = pipeline.handlers().store().take_batch();
    assert_eq!(batch.bots, vec![(2222, 77)]);
    assert_eq!(batch.events[0].occurred_at, "2026-09-30T04:00:01.000Z");
    db.store
        .commit_dispatch(&checkpoint("restart", 2, RESUME_URL), batch)
        .await
        .unwrap();
    let bot: bool = sqlx::query_scalar("SELECT is_bot FROM members WHERE member_id = '77'")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(bot);
    let restarted = crate::gateway::build_persistent_pipeline(
        &db.store,
        2222,
        TOKEN.into(),
        None,
        crate::voice_snapshot::new_shared_cache(),
    )
    .await
    .unwrap();
    assert_eq!(restarted.handlers().store().load(2222)[0].uses, 7);
    db.close().await;
}

async fn write_case(
    store: &GatewaySessionStore,
    saved: &GatewaySession,
    batch: FunnelBatch,
    with_job: bool,
) -> Result<DispatchAction, sqlx::Error> {
    if !with_job {
        return store.commit_dispatch(saved, batch).await;
    }
    let (action, id) = store
        .commit_dispatch_with_job(
            saved,
            batch,
            Some(two_bot_cutover::gateway_session::GatewayJob {
                payload: "{}".into(),
                occurred_at_ms: saved.updated_at_ms,
            }),
        )
        .await?;
    assert_eq!(id.is_some(), action == DispatchAction::Apply);
    if let Some(id) = id {
        let identity: (String, i64) =
            sqlx::query_as("SELECT session_id, seq FROM gateway_onboarding_jobs WHERE id=$1")
                .bind(id)
                .fetch_one(store.pool())
                .await?;
        assert_eq!(identity, (saved.session_id.clone(), saved.sequence as i64));
    }
    Ok(action)
}

async fn writer_cases(db: &TestDb, store: &GatewaySessionStore, url: &str, expected: &str) {
    for (with_job, baseline_uses, committed_uses) in [(false, 2, 3), (true, 3, 4)] {
        assert_eq!(
            store.invite_snapshots().await.unwrap()[0].uses,
            baseline_uses
        );
        store.clear().await.unwrap();
        sqlx::query("DELETE FROM gateway_onboarding_jobs")
            .execute(&db.pool)
            .await
            .unwrap();
        let saved = checkpoint("atomic", 1, url);
        let raw = || async {
            sqlx::query_as::<_, (String, i64, String, i64)>("SELECT session_id, seq, resume_url, floor(extract(epoch FROM updated_at)*1000)::bigint FROM gateway_sessions").fetch_one(&db.pool).await.unwrap()
        };
        let batch = |uses, fail| FunnelBatch {
            snapshots: vec![SnapshotWrite::StoreAll(2222, vec![invite(uses)])],
            events: if fail {
                vec![event(EventType::MemberLeave, "not-a-timestamp")]
            } else {
                vec![]
            },
            ..Default::default()
        };
        for (seq, uses, fail, action) in [
            (1, committed_uses, false, Some(DispatchAction::Apply)),
            (2, 99, true, None),
            (1, 99, false, Some(DispatchAction::Duplicate)),
        ] {
            let mut dispatch = saved.clone();
            dispatch.sequence = seq;
            dispatch.updated_at_ms += i64::from(uses == 99);
            assert_eq!(
                write_case(store, &dispatch, batch(uses, fail), with_job)
                    .await
                    .ok(),
                action
            );
            assert_eq!(
                raw().await,
                ("atomic".into(), 1, expected.into(), saved.updated_at_ms)
            );
            assert_eq!(
                store.invite_snapshots().await.unwrap()[0].uses,
                committed_uses
            );
            let jobs: Vec<(String, i64)> =
                sqlx::query_as("SELECT session_id, seq FROM gateway_onboarding_jobs")
                    .fetch_all(&db.pool)
                    .await
                    .unwrap();
            assert_eq!(
                jobs,
                if with_job {
                    vec![("atomic".into(), 1)]
                } else {
                    vec![]
                }
            );
        }
        let loaded = load_boot_session(store).await.unwrap();
        assert_eq!(loaded.is_some(), !expected.is_empty());
        if let Some(loaded) = loaded {
            assert!(loaded == saved, "valid fields survive boot unchanged");
        }
    }
}

async fn writer_policies(db: &TestDb) {
    use two_bot_core::gateway_session::ResumePolicy;
    for bootstrap in [None, Some("ws://127.0.0.1:1")] {
        let store = GatewaySessionStore::new(db.pool.clone(), GUILD.into(), 0);
        let store = if bootstrap.is_some() {
            store.with_resume_policy(ResumePolicy::for_bootstrap(bootstrap))
        } else {
            store
        };
        for url in [
            RESUME_URL,
            "ws://127.0.0.1:1234",
            "wss://remote.invalid",
            "ws://localhost:1234",
        ] {
            let expected =
                if url == RESUME_URL || (bootstrap.is_some() && url == "ws://127.0.0.1:1234") {
                    url
                } else {
                    ""
                };
            sqlx::query("UPDATE invite_snapshots SET uses = 2 WHERE guild_id = $1 AND code = $2")
                .bind(GUILD)
                .bind("fixture")
                .execute(&db.pool)
                .await
                .unwrap();
            writer_cases(db, &store, url, expected).await;
        }
    }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn received_invalid_endpoint_keeps_effects_but_not_usable_url() {
    let db = TestDb::new().await;
    let mut mock = MockGateway::scripted(false, false, None, true).await;
    let (mut runner, _) = spawn_runner(&db, &mock.url).await;
    let exercise = async {
        assert_eq!(mock.authentication().await["op"], 2);
        wait_sequence(&db.store, 2).await;
        let row: (String, i64, String) =
            sqlx::query_as("SELECT session_id,seq,resume_url FROM gateway_sessions")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(row, ("fresh-session".into(), 2, "".into()));
        assert_eq!(db.count().await, 1);
    };
    let result =
        std::panic::AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(25), exercise))
            .catch_unwind()
            .await;
    runner.abort();
    mock.task.abort();
    let runner_joined = tokio::time::timeout(Duration::from_secs(5), &mut runner).await;
    let mock_joined = tokio::time::timeout(Duration::from_secs(5), &mut mock.task).await;
    let runner_cleaned = matches!(&runner_joined, Ok(Ok(Ok(()))))
        || matches!(&runner_joined, Ok(Err(error)) if error.is_cancelled());
    let mock_cleaned = matches!(&mock_joined, Ok(Ok(())))
        || matches!(&mock_joined, Ok(Err(error)) if error.is_cancelled());
    let cleaned = runner_cleaned && mock_cleaned;
    if cleaned {
        db.close().await;
    } else {
        eprintln!("ingress teardown incomplete; retain schemas: runner={runner_joined:?}, mock={mock_joined:?}");
    }
    assert!(cleaned, "ingress teardown incomplete; schemas retained");
    match result {
        Ok(Ok(())) => {}
        Ok(Err(_)) => panic!("invalid ingress deadline"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
