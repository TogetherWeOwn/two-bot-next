//! Builder regressions for retry re-read failures; protected acceptance is unchanged.
#![cfg(test)]

use super::*;
use two_bot_testsupport::TestDatabase;

async fn next_pass(tickets: &TicketRuntime) -> std::result::Result<(), ErrorClass> {
    // Advance only between passes: real SQL and loopback I/O must not auto-timeout.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(300)).await;
    tokio::time::resume();
    tickets.recover().await
}

#[tokio::test]
async fn open_retry_store_read_failure_defers_then_repairs_once() {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("set the documented disposable agent-testdb bootstrap");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database");
    let broken = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let served = Arc::clone(&broken);
    let posted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let controls = Arc::clone(&posted);
    let mock = MockRest::with_responder(move |request| {
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/api/v10/channels/700") => ScriptedResponse::json(
                200,
                json!({"id":"700","guild_id":"100","type":0,
                    "permission_overwrites":[{"id":"400","type":1,"allow":"68624","deny":"0"}]}),
            ),
            ("GET", "/api/v10/channels/700/messages?limit=50") => ScriptedResponse::json(
                200,
                json!([{"id":"901","author":{"id":"400"},"components":[
                    {"type":1,"components":[{"type":2,"custom_id":TICKET_OPEN_ID}]}
                ]}]),
            ),
            ("GET", "/api/v10/channels/600") => {
                if served.load(Ordering::SeqCst) {
                    ScriptedResponse::status(503)
                } else {
                    ScriptedResponse::json(200, private_channel("two-ticket:store-retry"))
                }
            }
            ("GET", "/api/v10/channels/600/messages?limit=50") => ScriptedResponse::json(
                200,
                if controls.load(Ordering::SeqCst) {
                    json!([{"id":"900","author":{"id":"400"},"components":[
                        {"type":1,"components":[
                            {"type":2,"custom_id":TICKET_CLAIM_ID},
                            {"type":2,"custom_id":TICKET_CLOSE_ID}
                        ]}
                    ]}])
                } else {
                    json!([])
                },
            ),
            ("POST", "/api/v10/channels/600/messages") => {
                controls.store(true, Ordering::SeqCst);
                ScriptedResponse::json(200, json!({"id":"900"}))
            }
            _ => ScriptedResponse::status(500),
        }
    })
    .await;
    let tickets = runtime(db.pool().clone(), &mock);
    active(&tickets, "store-retry").await;
    assert!(tickets.recover().await.is_err());
    let row_calls = || {
        mock.requests()
            .iter()
            .filter(|r| r.path.starts_with("/api/v10/channels/600"))
            .count()
    };
    let before = row_calls();
    assert!(
        before > 0,
        "initial Open attempt reaches the faulted channel"
    );
    broken.store(false, Ordering::SeqCst);
    sqlx::query("ALTER TABLE tickets RENAME TO retry_hidden_tickets")
        .execute(db.pool())
        .await
        .unwrap();
    let sick = next_pass(&tickets).await;
    sqlx::query("ALTER TABLE retry_hidden_tickets RENAME TO tickets")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(matches!(sick, Err(ErrorClass::Database)));
    assert_eq!(row_calls(), before, "a sick re-read cannot attempt Discord");
    assert!(!posted.load(Ordering::SeqCst));
    assert!(tickets.recover().await.is_ok());
    assert_eq!(row_calls(), before, "the failed re-read must back off");
    let mut repaired = false;
    for _ in 0..12 {
        assert!(next_pass(&tickets).await.is_ok());
        if posted.load(Ordering::SeqCst) {
            repaired = true;
            break;
        }
    }
    let repaired_calls = row_calls();
    for _ in 0..3 {
        assert!(next_pass(&tickets).await.is_ok());
    }
    let settled_calls = row_calls();
    let writes: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method != "GET")
        .map(|r| (r.method, r.path))
        .collect();
    mock.shutdown().await;
    db.close()
        .await
        .expect("verify disposable database teardown");
    assert!(
        repaired,
        "a transient ticket re-read error must not shed the behind-cursor row"
    );
    assert_eq!(
        settled_calls, repaired_calls,
        "successful rows return to zero work"
    );
    assert_eq!(
        writes,
        vec![("POST".into(), "/api/v10/channels/600/messages".into())]
    );
}
