//! A total heartbeat-safe SQL deadline prevents pending-but-healthy zombies.
use super::*;

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn checkpoint_lock_wait_fails_closed_before_heartbeat_and_restart_recovers() {
    let db = TestDb::new().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let gateway_url = url.clone();
    let (send_dispatch, mut dispatch) = mpsc::channel::<()>(1);
    let (send_heartbeat, mut heartbeats) = mpsc::unbounded_channel();
    let mock = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (_, mut ws) = ServerBuilder::new().accept(stream).await.unwrap();
        ws.send(Message::text(
            json!({"op":10,"d":{"heartbeat_interval":1000}}).to_string(),
        ))
        .await
        .unwrap();
        loop {
            tokio::select! {
                packet = ws.next() => {
                    let Some(Ok(message)) = packet else { break };
                    if !message.is_text() { continue; }
                    let packet: Value = serde_json::from_str(message.as_text().unwrap()).unwrap();
                    match packet["op"].as_u64() {
                        Some(1) => {
                            send_heartbeat.send(()).unwrap();
                            ws.send(Message::text("{\"op\":11,\"d\":null}".to_owned())).await.unwrap();
                        }
                        Some(2) => {
                            ws.send(Message::text(ready(&gateway_url, "fresh-session").to_string())).await.unwrap();
                        }
                        _ => {}
                    }
                }
                Some(()) = dispatch.recv() => {
                    ws.send(Message::text(leave(2).to_string())).await.unwrap();
                }
            }
        }
    });
    let (runner, state) = spawn_runner(&db, &url).await;
    wait_sequence(&db.store, 1).await;
    // Prove this is an already-connected runner with working heartbeats.
    tokio::time::timeout(Duration::from_secs(3), heartbeats.recv())
        .await
        .unwrap()
        .unwrap();
    wait_connected(&state).await;
    let mut lock = db.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("gateway:{GUILD}:0"))
        .execute(&mut *lock)
        .await
        .unwrap();
    send_dispatch.send(()).await.unwrap();
    tokio::time::timeout(Duration::from_millis(200), async {
        while *state.read().await == GatewayState::Connected {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("readiness unavailable during blocked SQL");

    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let result = tokio::time::timeout(
        Duration::from_millis(700),
        crate::supervise_gateway(
            runner,
            async move {
                crate::server::shutdown_requested(receiver).await;
                Ok(())
            },
            state.clone(),
            shutdown,
        ),
    )
    .await
    .expect("fail closed before one 1000ms heartbeat interval")
    .expect_err("pending SQL must terminate the essential task");
    assert_eq!(
        result.to_string(),
        "gateway task stopped; container restart required"
    );
    assert_eq!(*state.read().await, GatewayState::Draining);
    assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 1);
    assert_eq!(db.count().await, 0);
    lock.rollback().await.unwrap();
    mock.abort();
    let _ = mock.await;
    // Cancellation must not continue the transaction after the lock is released.
    assert_eq!(db.store.load().await.unwrap().unwrap().sequence, 1);
    assert_eq!(db.count().await, 0);

    let mut second = MockGateway::new(false, true).await;
    sqlx::query("UPDATE gateway_sessions SET resume_url = $1")
        .bind(&second.url)
        .execute(&db.pool)
        .await
        .unwrap();
    let (runner, state) = spawn_runner(&db, &second.url).await;
    let auth = second.authentication().await;
    assert_eq!(auth["op"], 6);
    assert_eq!(auth["d"]["seq"], 1);
    wait_sequence(&db.store, 3).await;
    assert_eq!(db.count().await, 1, "missed dispatch replayed once");
    wait_connected(&state).await;
    runner.abort();
    let _ = runner.await;
    second.task.abort();
    db.close().await;
}
