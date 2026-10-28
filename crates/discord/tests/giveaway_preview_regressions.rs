//! Regression witnesses for the preview review: lock isolation, alias ambiguity
//! and checked deadline overflow. The separately protected acceptance stays intact.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use time::OffsetDateTime;
use twilight_model::application::interaction::Interaction;
use two_bot_discord::{giveaway::GiveawayRuntime, ActionExecutor};

const GUILD: &str = "1550102030405060701";
const CHANNEL: &str = "1550102030405060702";

fn invocation(options: Vec<Value>) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "1111", "authorizing_integration_owners": {"0": GUILD},
        "id": "9700", "token": "preview-regression", "type": 2, "version": 1,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"permissions": "32", "roles": [], "deaf": false, "mute": false,
            "flags": 0, "user": {"id": "1550102030405060711", "username": "staff", "discriminator": "0"}},
        "data": {"id": "4444", "name": "giveaway", "type": 1,
            "options": [{"name": "create", "type": 1, "options": options}]}
    })).expect("valid command")
}

fn options(preview: bool) -> Vec<Value> {
    vec![
        json!({"name": "prize", "type": 3, "value": "Friday prize"}),
        json!({"name": "duration", "type": 4, "value": 60}),
        json!({"name": "winners", "type": 4, "value": 2}),
        json!({"name": "preview", "type": 5, "value": preview}),
    ]
}

async fn tripwire() -> (PgPool, tokio::net::TcpListener) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("SQL tripwire");
    let connect = sqlx::postgres::PgConnectOptions::new()
        .host("127.0.0.1")
        .port(listener.local_addr().expect("bound port").port())
        .username("preview_fixture")
        .database("preview_fixture")
        .ssl_mode(sqlx::postgres::PgSslMode::Disable);
    (
        PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(15))
            .connect_lazy_with(connect),
        listener,
    )
}

async fn rest() -> MockRest {
    MockRest::with_responder(
        |request| match (request.method.as_str(), request.path.as_str()) {
            ("PATCH", "/api/v10/webhooks/1111/preview-regression/messages/@original") => {
                ScriptedResponse::json(200, json!({"id": "5900"}))
            }
            _ => ScriptedResponse::status(404),
        },
    )
    .await
}

fn runtime(pool: PgPool, mock: &MockRest) -> GiveawayRuntime {
    GiveawayRuntime::new(
        pool,
        ActionExecutor::with_proxy("mock-token-not-a-credential".into(), Some(mock.origin()))
            .expect("loopback executor"),
    )
}

fn assert_no_sql(listener: tokio::net::TcpListener) {
    assert!(matches!(listener.into_std().expect("listener").accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
}

#[tokio::test]
async fn preview_completes_while_a_live_tick_holds_the_effects_lock() {
    let (pool, listener) = tripwire().await;
    let mock = rest().await;
    let rt = Arc::new(runtime(pool.clone(), &mock));
    let ticking = rt.clone();
    let tick = tokio::spawn(async move { ticking.tick(GUILD).await });
    // tick() acquires EFFECTS before its first SQL query. Accepting this
    // connection proves the live effect is in progress; no sleeps/races.
    let (connection, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("tick reached store while holding lock")
        .expect("connection");
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        rt.command(&invocation(options(true))),
    )
    .await;
    let tick_still_blocked = !tick.is_finished();
    // Release the deliberately blocked effect only AFTER preview completion.
    tick.abort();
    let _ = tick.await;
    drop(connection);
    let requests = mock.requests();
    mock.shutdown().await;
    pool.close().await;
    result.expect("pure preview must not queue behind a live effect");
    assert!(
        tick_still_blocked,
        "preview completed before releasing the live effect"
    );
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "PATCH");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("JSON completion");
    assert!(body["content"]
        .as_str()
        .expect("text")
        .contains("Preview — not created"));
}

#[tokio::test]
async fn competing_duration_aliases_refuse_in_both_orders_even_for_real_create() {
    let (pool, listener) = tripwire().await;
    let mock = rest().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let rt = runtime(pool.clone(), &mock).with_create_sources(
        Arc::new(OffsetDateTime::now_utc),
        Arc::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            String::new()
        }),
    );
    for preview in [true, false] {
        for alias_first in [true, false] {
            let mut values = options(preview);
            let alias = json!({"name": "duration-minutes", "type": 4, "value": 30});
            if alias_first {
                values.insert(0, alias);
            } else {
                values.push(alias);
            }
            rt.command(&invocation(values)).await;
        }
    }
    let requests = mock.requests();
    mock.shutdown().await;
    pool.close().await;
    assert_no_sql(listener);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(requests.len(), 4);
    for request in requests {
        assert_eq!(request.method, "PATCH", "no public posts");
        let body: Value = serde_json::from_slice(&request.body).expect("JSON completion");
        let text = body["content"].as_str().expect("refusal");
        assert!(text.contains("Invalid") && text.contains("duration"));
        assert!(!text.contains("Preview — not created"));
    }
}

#[tokio::test]
async fn overflowing_deadline_refuses_before_randomness_or_sql_for_preview_and_create() {
    let (pool, listener) = tripwire().await;
    let mock = rest().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let near_max = time::Date::MAX
        .with_hms(23, 59, 59)
        .expect("last second")
        .assume_utc();
    let rt = runtime(pool.clone(), &mock).with_create_sources(
        Arc::new(move || near_max),
        Arc::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            String::new()
        }),
    );
    for preview in [true, false] {
        rt.command(&invocation(options(preview))).await;
    }
    let requests = mock.requests();
    mock.shutdown().await;
    pool.close().await;
    assert_no_sql(listener);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(request.method, "PATCH", "no public posts");
        let body: Value = serde_json::from_slice(&request.body).expect("JSON completion");
        assert!(body["content"]
            .as_str()
            .expect("refusal")
            .contains("Invalid"));
    }
}

#[tokio::test]
async fn stalled_preview_completion_does_not_hold_up_a_live_tick() {
    let (pool, listener) = tripwire().await;
    let (mock, gate) = MockRest::start_gated(
        vec![ScriptedResponse::json(200, json!({"id": "5900"}))],
        ScriptedResponse::status(404),
    )
    .await;
    let rt = Arc::new(runtime(pool.clone(), &mock));
    let previewing = rt.clone();
    let preview = tokio::spawn(async move {
        previewing.command(&invocation(options(true))).await;
    });
    tokio::time::timeout(Duration::from_secs(5), gate.wait_for_request())
        .await
        .expect("preview reached its private completion");
    let ticking = rt.clone();
    let tick = tokio::spawn(async move { ticking.tick(GUILD).await });
    let connection = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await;
    let preview_still_blocked = !preview.is_finished();
    // Both witnesses are captured before releasing the private webhook.
    tick.abort();
    let _ = tick.await;
    gate.release();
    preview.await.expect("preview finished after release");
    mock.shutdown().await;
    pool.close().await;
    connection
        .expect("live tick must not queue behind preview completion")
        .expect("tick reached store");
    assert!(preview_still_blocked);
}
