//! Review regressions: rejected closes, URL fallback and persistence failure.
use super::*;

async fn rejected_close(code: u16) {
    let db = TestDb::new().await;
    let mut mock = MockGateway::with_close(false, false, Some(code)).await;
    db.store
        .commit_dispatch(
            &checkpoint("rejected", 42, &mock.url),
            FunnelBatch::default(),
        )
        .await
        .expect("seed");
    let (runner, state) = spawn_runner(&db, &mock.url).await;
    assert_eq!(mock.authentication().await["op"], 6);
    let next = mock.authentication().await;
    assert_eq!(next["op"], 2, "close {code} requires IDENTIFY");
    wait_sequence(&db.store, 2).await;
    let saved = db.store.load().await.unwrap().unwrap();
    assert_eq!(saved.session_id, "fresh-session");
    assert_eq!(saved.resume_url, mock.url);
    assert_eq!(db.count().await, 1);
    wait_connected(&state).await;
    assert!(!runner.is_finished());
    runner.abort();
    let _ = runner.await;
    mock.task.abort();
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn close_4007_identifies_and_replaces_checkpoint() {
    rejected_close(4007).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn close_4009_identifies_and_replaces_checkpoint() {
    rejected_close(4009).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn failed_saved_endpoint_resumes_and_commits_with_saved_ready_url() {
    let db = TestDb::new().await;
    let mut mock = MockGateway::new(false, true).await;
    let saved_url = "ws://127.0.0.1:1";
    db.store
        .commit_dispatch(
            &checkpoint("fresh-session", 2, saved_url),
            FunnelBatch::default(),
        )
        .await
        .expect("seed");
    let (runner, state) = spawn_runner(&db, &mock.url).await;
    let auth = mock.authentication().await;
    assert_eq!(auth["op"], 6);
    assert_eq!(auth["d"]["session_id"], "fresh-session");
    assert_eq!(auth["d"]["seq"], 2);
    wait_sequence(&db.store, 3).await;
    let saved = db.store.load().await.unwrap().unwrap();
    assert_eq!(saved.resume_url, saved_url);
    assert_eq!(db.count().await, 0, "replayed leave must be skipped");
    wait_connected(&state).await;
    assert!(!runner.is_finished());
    runner.abort();
    let _ = runner.await;
    mock.task.abort();
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn persistence_failure_stops_service_and_restart_recovers_committed_sequence() {
    let db = TestDb::new().await;
    // Inject a DB write outage in this test's isolated schema only. READY can
    // commit, but the next dispatch fails when inserting its funnel event.
    sqlx::query(
        "CREATE FUNCTION reject_event() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'injected test write failure'; END $$",
    )
    .execute(&db.pool)
    .await
    .expect("failure function");
    sqlx::query(
        "CREATE TRIGGER reject_event BEFORE INSERT ON events
         FOR EACH ROW EXECUTE FUNCTION reject_event()",
    )
    .execute(&db.pool)
    .await
    .expect("failure trigger");
    let mut first = MockGateway::new(false, false).await;
    let (runner, state) = spawn_runner(&db, &first.url).await;
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        crate::supervise_gateway(
            runner,
            async move {
                crate::server::shutdown_requested(receiver).await;
                Ok(())
            },
            shutdown,
        ),
    )
    .await
    .expect("service must stop")
    .expect_err("container exits nonzero instead of serving health indefinitely");
    assert_eq!(
        result.to_string(),
        "gateway task stopped; container restart required"
    );
    assert_eq!(first.authentication().await["op"], 2);
    assert_eq!(*state.read().await, GatewayState::Armed);
    let saved = db.store.load().await.unwrap().unwrap();
    assert_eq!(
        saved.sequence, 1,
        "failed dispatch must not advance checkpoint"
    );
    assert_eq!(db.count().await, 0, "failed funnel insertion rolled back");
    first.task.abort();

    sqlx::query("DROP TRIGGER reject_event ON events")
        .execute(&db.pool)
        .await
        .expect("test DB recovered");
    // Simulate the Container restarting after DB recovery: no old pipeline
    // state or uncommitted sequence is reused.
    let mut second = MockGateway::new(false, true).await;
    sqlx::query("UPDATE gateway_sessions SET resume_url = $1")
        .bind(&second.url)
        .execute(&db.pool)
        .await
        .expect("mock endpoint");
    let (runner, state) = spawn_runner(&db, &second.url).await;
    let auth = second.authentication().await;
    assert_eq!(auth["op"], 6);
    assert_eq!(auth["d"]["seq"], 1);
    wait_sequence(&db.store, 3).await;
    assert_eq!(
        db.count().await,
        1,
        "missed dispatch persisted exactly once"
    );
    wait_connected(&state).await;
    assert!(!runner.is_finished());
    runner.abort();
    let _ = runner.await;
    second.task.abort();
    db.close().await;
}
