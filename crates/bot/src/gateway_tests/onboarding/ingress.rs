//! Receive-relative ACK and ordered-commit fences through real mock sockets.
use super::*;

async fn wait_response(mock: &MockRest, path: &str) -> crate::discord_test_common::RestResponse {
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Some(response) = mock
                .responses()
                .into_iter()
                .find(|response| response.path == path)
            {
                return response;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("response written to mock socket")
}

async fn lock_gateway(db: &TestDb) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut lock = db.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("gateway:{GUILD}:0"))
        .execute(&mut *lock)
        .await
        .unwrap();
    lock
}

async fn wait_unready(runner: &Runner) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while *runner.state.read().await == GatewayState::Connected {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("ordered checkpoint SQL pending");
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn onboarding_gateway_ingress_ack_precedes_blocked_sql_but_selection_waits_for_commit() {
    let db = TestDb::with_pool_max(crate::gateway::FEATURE_POOL_MAX).await;
    let gateway_pool = db.independent_pool(crate::gateway::GATEWAY_POOL_MAX).await;
    let store = GatewaySessionStore::new(gateway_pool.clone(), GUILD.into(), 0);
    let mock = discord(Arc::new(AtomicBool::new(false)), PauseAt::PermissionRead).await;
    let result = bounded(async {
        let mut ws = gateway(false).await;
        let runner = spawn_onboarding_with_store(&db, &mock, &ws.mock.url, "session", &store).await;
        assert_eq!(ws.mock.authentication().await["op"], 2);
        wait_sequence(&store, 1).await;
        let lock = lock_gateway(&db).await;
        ws.send(leave(2)).await;
        wait_unready(&runner).await;
        let wire_at = ws.send(component(3, "6103", "ingress-test-token")).await;
        let callback_path = "/api/v10/interactions/6103/ingress-test-token/callback";
        let response = wait_response(&mock, callback_path).await;
        assert_eq!(response.status, 204);
        assert!(
            response.sent_at.duration_since(wire_at) < Duration::from_millis(2500),
            "actual WebSocket send to accepted callback response, not enqueue time"
        );
        let callback = wait_request(&mock, "POST", callback_path).await;
        let body: Value = serde_json::from_slice(&callback.body).unwrap();
        assert_eq!(body["type"], 5);
        assert_eq!(body["data"]["flags"], 64);
        assert!(callback.header("authorization").is_none());
        // Hold preceding ordinary-dispatch SQL beyond Discord's ACK deadline.
        tokio::time::sleep_until(
            tokio::time::Instant::from_std(wire_at) + Duration::from_millis(3100),
        )
        .await;
        let saved: i64 = sqlx::query_scalar("SELECT seq FROM gateway_sessions")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(saved, 1);
        assert_eq!(count_jobs(&db).await, 0);
        assert_eq!(event_count(&db, EVENT_CHANNEL_ROUTED).await, 0);
        assert_eq!(event_count(&db, EVENT_GAME_ROLES_SELECTED).await, 0);
        assert_eq!(
            mock.requests().len(),
            1,
            "ACK only: no settings-driven permission/role/reply effect before COMMIT"
        );
        lock.rollback().await.unwrap();
        wait_sequence(&store, 3).await;
        wait_receipt(&db, 3, "completed").await;
        assert_eq!(event_count(&db, EVENT_CHANNEL_ROUTED).await, 1);
        assert_eq!(event_count(&db, EVENT_GAME_ROLES_SELECTED).await, 0);
        assert!(!mock
            .requests()
            .iter()
            .any(|r| matches!(r.method.as_str(), "PUT" | "DELETE")));
        // Replay is fenced before ACK as well as transactionally at persistence.
        ws.send(component(3, "6103", "ingress-test-token")).await;
        ws.send(resumed(4)).await;
        wait_sequence(&store, 4).await;
        assert_eq!(
            mock.requests()
                .iter()
                .filter(|r| r.path == callback_path)
                .count(),
            1
        );
        runner.stop().await;
    })
    .await;
    gateway_pool.close().await;
    cleanup(db, Some(mock), result).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn onboarding_gateway_ingress_settings_timeout_finishes_confirmed_defer_without_effects() {
    let db = TestDb::new().await;
    let mock = discord(Arc::new(AtomicBool::new(false)), PauseAt::PermissionRead).await;
    let result = bounded(async {
        let mut ws = gateway(false).await;
        let runner = spawn_onboarding(&db, &mock, &ws.mock.url, "legacy").await;
        assert_eq!(ws.mock.authentication().await["op"], 2);
        wait_sequence(&db.store, 1).await;
        let mut lock = db.pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE guild_settings IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *lock)
            .await
            .unwrap();
        let mut click = component(2, "6202", "settings-test-token");
        click["d"]["data"]["custom_id"] = json!(GAME_SELECT_ID);
        click["d"]["data"]["values"] = json!([GAME_PICKS[0].key]);
        let wire_at = ws.send(click).await;
        let ack = wait_response(
            &mock,
            "/api/v10/interactions/6202/settings-test-token/callback",
        )
        .await;
        assert_eq!(ack.status, 204);
        assert!(ack.sent_at.duration_since(wire_at) < Duration::from_millis(2500));
        wait_sequence(&db.store, 2).await;
        let reply = wait_request(
            &mock,
            "PATCH",
            "/api/v10/webhooks/1111/settings-test-token/messages/@original",
        )
        .await;
        assert!(
            reply.received_at.duration_since(ack.sent_at) < Duration::from_secs(7),
            "bounded error edit includes 1.5s settings and 5s reply budgets"
        );
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert!(body["content"]
            .as_str()
            .unwrap()
            .contains("No selection was applied"));
        wait_receipt(&db, 2, "completed").await;
        assert_eq!(event_count(&db, EVENT_GAME_ROLES_SELECTED).await, 0);
        assert_eq!(event_count(&db, EVENT_CHANNEL_ROUTED).await, 0);
        assert_eq!(
            mock.requests().len(),
            2,
            "one callback and one honest edit, no member/role reads or writes"
        );
        lock.rollback().await.unwrap();
        runner.stop().await;
    })
    .await;
    cleanup(db, Some(mock), result).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn onboarding_gateway_ingress_overflow_fails_closed_and_cancels_pending_checkpoint() {
    let db = TestDb::new().await;
    let mock = discord(Arc::new(AtomicBool::new(false)), PauseAt::PermissionRead).await;
    let result = bounded(async {
        let mut ws = gateway(false).await;
        let mut runner = spawn_onboarding(&db, &mock, &ws.mock.url, "legacy").await;
        assert_eq!(ws.mock.authentication().await["op"], 2);
        wait_sequence(&db.store, 1).await;
        let lock = lock_gateway(&db).await;
        ws.send(leave(2)).await;
        wait_unready(&runner).await;
        // One more than the bounded writer backlog (DISPATCH_BACKLOG) plus the
        // dispatch the writer already holds.
        for seq in 3..=(3 + crate::dispatch::DISPATCH_BACKLOG as u64 + 1) {
            ws.send(resumed(seq)).await;
        }
        let error = (&mut runner.task)
            .await
            .expect("owner must not panic")
            .expect_err("finite ingress must fail closed");
        assert!(error.to_string().contains("dispatch"));
        assert_ne!(*runner.state.read().await, GatewayState::Connected);
        lock.rollback().await.unwrap();
        assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 1);
        assert_eq!(
            db.count().await,
            0,
            "cancelled SQL cannot commit after lock release"
        );
        assert_eq!(count_jobs(&db).await, 0);
        assert!(mock.requests().is_empty());
    })
    .await;
    cleanup(db, Some(mock), result).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn onboarding_gateway_ingress_unconfirmed_ack_has_no_selection_effects_or_second_callback() {
    let db = TestDb::new().await;
    let mock = discord(Arc::new(AtomicBool::new(true)), PauseAt::Callback).await;
    let result = bounded(async {
        let mut ws = gateway(false).await;
        let runner = spawn_onboarding(&db, &mock, &ws.mock.url, "session").await;
        assert_eq!(ws.mock.authentication().await["op"], 2);
        wait_sequence(&db.store, 1).await;
        let wire_at = ws.send(component(2, "6402", "slow-ack-test-token")).await;
        wait_sequence(&db.store, 2).await;
        let reply = wait_request(
            &mock,
            "PATCH",
            "/api/v10/webhooks/1111/slow-ack-test-token/messages/@original",
        )
        .await;
        assert!(reply.received_at.duration_since(wire_at) < Duration::from_secs(8));
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert!(body["content"]
            .as_str()
            .unwrap()
            .contains("couldn't confirm"));
        wait_receipt(&db, 2, "completed").await;
        assert_eq!(mock.requests().len(), 2);
        assert_eq!(event_count(&db, EVENT_CHANNEL_ROUTED).await, 0);
        assert_eq!(event_count(&db, EVENT_GAME_ROLES_SELECTED).await, 0);
        assert!(
            !mock
                .responses()
                .iter()
                .any(|r| r.path.ends_with("/callback")),
            "delayed callback was never accepted inside the ACK budget"
        );
        runner.stop().await;
    })
    .await;
    cleanup(db, Some(mock), result).await;
}
