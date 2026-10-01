//! S5/S6 integration: all effects, including invite baselines and bot flags,
//! must roll back with a failed checkpoint. Only test-container schemas.
use super::*;
use two_bot_core::gateway_funnel::SnapshotWrite;
use two_bot_core::{InviteSnapshotStore, InviteState};

fn invite(uses: u64) -> InviteState {
    InviteState {
        code: "fixture".into(),
        uses,
        inviter_id: Some(9),
        channel_id: Some(8),
    }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn snapshot_bot_activity_and_checkpoint_commit_or_roll_back_together() {
    let db = TestDb::new().await;
    two_bot_store::migrate(&db.pool).await.unwrap();
    two_bot_store::apply_web_contract(&db.pool).await.unwrap();
    db.store
        .commit_dispatch(
            &checkpoint("atomic", 1, "ws://mock"),
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
        activity: vec![(2222, 88, "2026-09-30T04:00:00.000Z".into())],
        events: vec![event(EventType::MemberLeave, "not-a-timestamp")],
        ..Default::default()
    };
    assert!(db
        .store
        .commit_dispatch(&checkpoint("atomic", 2, "ws://mock"), bad)
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
            &checkpoint("atomic", 2, "ws://mock"),
            FunnelBatch {
                bots: vec![(2222, 77)],
                snapshots: vec![SnapshotWrite::StoreAll(2222, vec![invite(2)])],
                activity: vec![(2222, 88, "2026-09-30T04:00:00.000Z".into())],
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
                &checkpoint("atomic", 2, "ws://mock"),
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
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn persistent_pipeline_hydrates_invites_and_stages_bot_departure_on_restart() {
    let db = TestDb::new().await;
    db.store
        .commit_dispatch(
            &checkpoint("restart", 1, "ws://mock"),
            FunnelBatch {
                snapshots: vec![SnapshotWrite::StoreAll(2222, vec![invite(7)])],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let pipeline = crate::gateway::build_persistent_pipeline(&db.store, 2222, TOKEN.into())
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
    pipeline.handle_at(&remove, "2026-09-30T04:00:01.000Z");
    let batch = pipeline.handlers().store().take_batch();
    assert_eq!(batch.bots, vec![(2222, 77)]);
    assert_eq!(batch.events[0].occurred_at, "2026-09-30T04:00:01.000Z");
    db.store
        .commit_dispatch(&checkpoint("restart", 2, "ws://mock"), batch)
        .await
        .unwrap();
    let bot: bool = sqlx::query_scalar("SELECT is_bot FROM members WHERE member_id = '77'")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(bot);
    let restarted = crate::gateway::build_persistent_pipeline(&db.store, 2222, TOKEN.into())
        .await
        .unwrap();
    assert_eq!(restarted.handlers().store().load(2222)[0].uses, 7);
    db.close().await;
}
