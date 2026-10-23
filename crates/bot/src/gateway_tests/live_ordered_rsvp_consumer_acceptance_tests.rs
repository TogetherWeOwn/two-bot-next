//! Protected acceptance: published settings travel through actual WebSocket
//! reception, ordered RSVP admission, checkpoint and completion, not prepare(bool).
//! Mutant: hardcode true at gateway reception's runtime.prepare call.
//!
//! Builder seam: InteractionRuntime::set_live_settings(&self, LiveSettings),
//! with an accessor used by actual gateway reception before preparing work.
//! The instance is the separately composed ORDERED runtime, not the command
//! runtime's internal copy. No process-global settings changes.

#![cfg(test)]

use super::*;
use two_bot_core::settings::{live_channel, LiveSettings, SettingRow, SettingsSnapshot};
use two_bot_core::ANNOUNCEMENTS_DISABLED_REPLY;
use two_bot_testsupport::TestDatabase;

trait BaselineOrderedSettings {
    fn set_live_settings(&self, live: LiveSettings);
}
impl BaselineOrderedSettings for InteractionRuntime {
    fn set_live_settings(&self, _live: LiveSettings) {}
}

#[tokio::test]
async fn ordered_gateway_rsvp_observes_off_on_delete_and_foreign_snapshots() {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        eprintln!("SKIP ordered RSVP consumer acceptance: TWO_TEST_DATABASE_URL not set");
        return;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable test database");
    let store = GatewaySessionStore::new(db.pool().clone(), GUILD.into(), 0);
    let (terminal_tx, mut terminal_rx) = tokio::sync::mpsc::unbounded_channel();
    let rest = MockRest::with_responder(move |request| {
        if request.path.ends_with("/callback") {
            let body: Value = serde_json::from_slice(&request.body).expect("callback JSON");
            if body["type"] == 4 {
                terminal_tx
                    .send(request.path.clone())
                    .expect("terminal callback receiver");
            }
            return ScriptedResponse::status(204);
        }
        if request.method == "PATCH" && request.path.ends_with("/messages/@original") {
            terminal_tx
                .send(request.path.clone())
                .expect("completion receiver");
            return ScriptedResponse::json(200, json!({"id":"99"}));
        }
        if request.method == "GET" && request.path.contains("/members/") {
            return ScriptedResponse::json(200, json!({"user":{"id":"77"},"roles":[]}));
        }
        if request.method == "GET" && request.path.contains("scheduled-events") {
            return ScriptedResponse::json(200, json!({"id":EVENT,"guild_id":GUILD,"status":1}));
        }
        ScriptedResponse::json(200, json!({"id":"1111"}))
    })
    .await;
    let ordered = Arc::new(InteractionRuntime::with_router(
        InteractionRouter::new(gates()),
        db.pool().clone(),
        ActionExecutor::with_proxy(TOKEN.into(), Some(rest.origin())).expect("loopback executor"),
        0,
        ClassifierConfig::default(),
    ));
    let (mut writer, live) = live_channel();
    // Qualified compatibility call keeps the baseline adapter live under
    // -D warnings once the inherent instance setter shadows the next call.
    <InteractionRuntime as BaselineOrderedSettings>::set_live_settings(&ordered, live.clone());
    ordered.set_live_settings(live);

    ensure_crypto_provider();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback gateway");
    let gateway_url = format!("ws://{}", listener.local_addr().expect("gateway address"));
    let shard =
        crate::gateway::build_shard(TOKEN.into(), Intents::empty(), None, Some(&gateway_url));
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let mut runner = tokio::spawn(run_shard(
        shard,
        Arc::new(build_pipeline(
            store.milestones().await.expect("milestones"),
            None,
        )),
        Arc::new(RwLock::new(GatewayState::Armed)),
        store.clone(),
        Some(ordered),
        None,
        None,
        None,
        None,
        crate::server::shutdown_requested(receiver),
    ));
    let (socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("gateway connect deadline")
        .expect("gateway socket");
    let (_, mut ws) = ServerBuilder::new()
        .accept(socket)
        .await
        .expect("websocket handshake");
    ws.send(Message::text(
        json!({"op":10,"d":{"heartbeat_interval":45000}}).to_string(),
    ))
    .await
    .expect("HELLO");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = ws
                .next()
                .await
                .expect("client frame")
                .expect("valid client frame");
            if !frame.is_text() {
                continue;
            }
            let packet: Value =
                serde_json::from_str(frame.as_text().expect("text")).expect("packet");
            if packet["op"] == 2 {
                break;
            }
            if packet["op"] == 1 {
                ws.send(Message::text("{\"op\":11,\"d\":null}".to_owned()))
                    .await
                    .expect("heartbeat ACK");
            }
        }
    })
    .await
    .expect("IDENTIFY deadline");
    ws.send(Message::text(
        ready(&gateway_url, "live-settings-session").to_string(),
    ))
    .await
    .expect("READY");
    wait_sequence(&store, 1).await;

    let mut observed = Vec::new();
    for (sequence, guild, value, status) in [
        (2, GUILD, Some(json!(0)), "going"),
        (3, GUILD, Some(json!(1)), "going"),
        (4, GUILD, None, "interested"),
        (5, GUILD, Some(json!(0)), "going"),
        (6, "9999", Some(json!(0)), "going"),
    ] {
        writer.publish(&SettingsSnapshot {
            revision: sequence as i64,
            rows: value
                .map(|value| SettingRow {
                    guild_id: guild.into(),
                    key: "TWO_ANNOUNCEMENTS".into(),
                    value,
                    version: sequence as i64,
                })
                .into_iter()
                .collect(),
        });
        ws.send(Message::text(interaction(sequence, status).to_string()))
            .await
            .expect("RSVP packet");
        let token = format!("mock-rsvp-{sequence}");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let path = terminal_rx.recv().await.expect("terminal response");
                if path.contains(&token) {
                    break;
                }
            }
        })
        .await
        .expect("ordered RSVP completion deadline");
        wait_sequence(&store, sequence).await;
        let requests = rest.requests();
        let callbacks: Vec<_> = requests
            .iter()
            .filter(|r| r.path.contains(&token) && r.path.ends_with("/callback"))
            .collect();
        assert_eq!(
            callbacks.len(),
            1,
            "one initial response for sequence {sequence}"
        );
        let body: Value = serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
        if body["type"] == 4 {
            assert_eq!(body["data"]["content"], ANNOUNCEMENTS_DISABLED_REPLY);
            assert_eq!(body["data"]["flags"], 64);
            assert!(
                !requests
                    .iter()
                    .any(|r| r.path.contains(&token) && r.method == "PATCH"),
                "disabled RSVP must not defer/complete"
            );
        }
        let row: Option<String> = sqlx::query_scalar("SELECT status FROM event_rsvps")
            .fetch_optional(db.pool())
            .await
            .expect("durable RSVP status");
        let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM announcements_audit_log")
            .fetch_one(db.pool())
            .await
            .expect("durable RSVP audits");
        observed.push((body["type"].as_u64().expect("response type"), row, audits));
    }
    shutdown.send_replace(true);
    let stopped = tokio::time::timeout(Duration::from_secs(10), &mut runner).await;
    let clean_shutdown = match stopped {
        Ok(result) => result.is_ok_and(|result| result.is_ok()),
        Err(_) => {
            runner.abort();
            let _ = runner.await;
            false
        }
    };
    db.close().await.expect("owned fixture teardown");
    assert!(
        clean_shutdown,
        "ordered gateway must drain and stop cleanly"
    );
    assert_eq!(observed, vec![
        (4, None, 0),
        (5, Some("going".into()), 1),
        (5, Some("interested".into()), 2),
        (4, Some("interested".into()), 2),
        (5, Some("going".into()), 3),
    ], "real ordered ingress must refuse own off rows without a write or audit and restore boot on delete/foreign rows");
}
