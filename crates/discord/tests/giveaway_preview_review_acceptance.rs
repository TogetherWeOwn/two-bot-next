//! Independent review regressions; preserve these tests in the combined fix.
//! Catches previews waiting for/retaining the live-effect lock, loss of duration
//! alias collision rejection, and unchecked deadline overflow. Local wire doubles
//! only: the SQL sockets never answer PostgreSQL or connect to a database.

#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use time::OffsetDateTime;
use tokio::{net::TcpListener, task::JoinSet};
use twilight_model::application::interaction::Interaction;
use two_bot_core::commands::PERM_MANAGE_GUILD;
use two_bot_discord::{giveaway::GiveawayRuntime, ActionExecutor};

const GUILD: &str = "1550102030405060701";
const CHANNEL: &str = "1550102030405060702";
const COMPLETION: &str = "/api/v10/webhooks/1111/preview-review-fixture/messages/@original";
const BOUND: Duration = Duration::from_secs(3);
const SQL_HOLD: Duration = Duration::from_secs(60);

fn invocation(id: u64, preview: Option<bool>, durations: &[&str]) -> Interaction {
    let mut options = vec![
        json!({"name": "prize", "type": 3, "value": "Review prize"}),
        json!({"name": "winners", "type": 4, "value": 1}),
    ];
    for (index, name) in durations.iter().enumerate() {
        // Unequal, individually valid values distinguish both precedence orders.
        options.push(json!({"name": name, "type": 4, "value": index + 1}));
    }
    if let Some(value) = preview {
        options.push(json!({"name": "preview", "type": 5, "value": value}));
    }
    serde_json::from_value(json!({
        "application_id": "1111", "authorizing_integration_owners": {"0": GUILD},
        "id": id.to_string(), "token": "preview-review-fixture", "type": 2, "version": 1,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"permissions": PERM_MANAGE_GUILD.to_string(), "roles": [],
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": "1550102030405060711", "username": "staff", "discriminator": "0"}},
        "data": {"id": "4444", "name": "giveaway", "type": 1,
            "options": [{"name": "create", "type": 1, "options": options}]}
    }))
    .expect("authorized valid Discord wire fixture")
}

async fn tripwire() -> (PgPool, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("SQL observer");
    let options = sqlx::postgres::PgConnectOptions::new()
        .host("127.0.0.1")
        .port(listener.local_addr().expect("bound port").port())
        .username("preview_review_fixture")
        .database("preview_review_fixture")
        .ssl_mode(sqlx::postgres::PgSslMode::Disable);
    let pool = PgPoolOptions::new()
        .min_connections(0)
        .max_connections(1)
        .acquire_timeout(SQL_HOLD)
        .connect_lazy_with(options);
    (pool, listener)
}

fn no_sql(listener: TcpListener) {
    let listener = listener.into_std().expect("nonblocking SQL observer");
    assert!(
        matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "unexpected SQL: store, admission, capacity and audit are all forbidden"
    );
}

async fn rest() -> MockRest {
    MockRest::with_responder(
        |request| match (request.method.as_str(), request.path.as_str()) {
            ("PATCH", COMPLETION) => ScriptedResponse::json(200, json!({"id": "5900"})),
            _ => ScriptedResponse::status(404),
        },
    )
    .await
}

fn ordinary_now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("fixed valid clock")
}

fn runtime(
    pool: &PgPool,
    mock: &MockRest,
    now: OffsetDateTime,
) -> (GiveawayRuntime, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let executor =
        ActionExecutor::with_proxy("mock-token-not-a-credential".into(), Some(mock.origin()))
            .expect("local REST executor");
    let runtime = GiveawayRuntime::new(pool.clone(), executor).with_create_sources(
        Arc::new(move || now),
        Arc::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            format!("{:032x}", std::process::id())
        }),
    );
    (runtime, calls)
}

// Real post-authorization command seam. Its dispatcher already deferred privately;
// @original edits inherit that privacy. No dispatcher/production hooks are replaced.
fn private_completion(requests: &[RestRequest]) -> Value {
    assert_eq!(
        requests.len(),
        1,
        "exactly one completion; no public/member/component/followup effect"
    );
    assert_eq!(requests[0].method, "PATCH");
    assert_eq!(
        requests[0].path, COMPLETION,
        "only the deferred private original"
    );
    let body: Value = serde_json::from_slice(&requests[0].body).expect("completion JSON");
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    for kind in ["users", "roles"] {
        assert!(
            body["allowed_mentions"][kind].is_null() || body["allowed_mentions"][kind] == json!([])
        );
    }
    assert_ne!(body["allowed_mentions"]["replied_user"], true);
    assert!(body["components"].is_null() || body["components"] == json!([]));
    let text = body["content"].as_str().expect("visible private content");
    assert!(!text.is_empty() && text.encode_utf16().count() <= 2000);
    assert!(!text.contains("gw:") && !text.contains("Nonce:") && !text.contains("Seed:"));
    body
}

fn assert_preview(requests: &[RestRequest]) {
    let reply = private_completion(requests);
    let text = reply["content"].as_str().expect("preview content");
    assert!(
        text.contains("Preview — not created"),
        "not a successful preview: {text}"
    );
    assert!(text.contains("Review prize"), "proposal was not rendered");
    assert!(
        text.contains("<t:1800000060:F>") || text.contains("<t:1800000060:f>"),
        "preview must use the valid injected clock and one-minute duration: {text}"
    );
}

fn assert_refusal(requests: &[RestRequest]) {
    let reply = private_completion(requests);
    let text = reply["content"].as_str().expect("refusal content");
    assert!(
        text.to_lowercase().contains("invalid"),
        "expected validation refusal, not unavailable/success: {text}"
    );
    assert!(!text.contains("Preview — not created"));
}

#[tokio::test]
async fn preview_completes_before_a_held_live_effect_is_released() {
    let (live_pool, live_listener) = tripwire().await;
    let live_mock = rest().await;
    let (live, _) = runtime(&live_pool, &live_mock, ordinary_now());
    // Separate runtime, pool and executor: only the real process-wide effect lock
    // can couple this live create to the later preview.
    let mut tasks = JoinSet::new();
    let live_task = tasks.spawn(async move {
        live.command(&invocation(9810, Some(false), &["duration"]))
            .await;
    });
    let acquired = tokio::time::timeout(BOUND, live_listener.accept()).await;
    if !matches!(&acquired, Ok(Ok(_))) {
        tasks.shutdown().await;
        live_pool.close().await;
        live_mock.shutdown().await;
        panic!("fixture startup: live create never reached SQL; not preview-lock RED");
    }
    // Accept is a positive acquisition witness, not a sleep: SQL is downstream
    // of the live-effect lock. Retain the socket without replying to PostgreSQL.
    let (held_socket, _) = acquired
        .expect("observed acquisition")
        .expect("accepted socket");
    let live_held_before = !live_task.is_finished();
    let (preview_pool, preview_listener) = tripwire().await;
    let preview_mock = rest().await;
    let (preview, nonce_calls) = runtime(&preview_pool, &preview_mock, ordinary_now());
    let interaction = invocation(9811, Some(true), &["duration"]);
    let completed = tokio::time::timeout(BOUND, preview.command(&interaction))
        .await
        .is_ok();
    let live_held_after = !live_task.is_finished();
    let requests = preview_mock.requests();
    let live_requests = live_mock.requests();
    // Clean up before asserting the expected RED, including cancellation of SQLx
    // acquisition and release of its effect guard. JoinSet also aborts on panic.
    tasks.shutdown().await;
    drop(held_socket);
    drop(live_listener);
    live_pool.close().await;
    preview_pool.close().await;
    live_mock.shutdown().await;
    preview_mock.shutdown().await;
    assert!(
        live_held_before && live_held_after,
        "fixture lost the held live effect"
    );
    assert!(
        live_requests.is_empty(),
        "live create unexpectedly finished its store attempt"
    );
    no_sql(preview_listener);
    assert_eq!(
        nonce_calls.load(Ordering::SeqCst),
        0,
        "preview minted randomness"
    );
    assert!(
        completed,
        "preview waited for the live-effect lock despite requiring no effects"
    );
    assert_preview(&requests);
}

#[tokio::test]
async fn held_preview_completion_does_not_retain_the_live_effect_lock() {
    let (preview_pool, preview_listener) = tripwire().await;
    let (preview_mock, gate) = MockRest::start_gated(
        vec![ScriptedResponse::json(200, json!({"id": "5900"}))],
        ScriptedResponse::status(404),
    )
    .await;
    let (preview, nonce_calls) = runtime(&preview_pool, &preview_mock, ordinary_now());
    let mut tasks = JoinSet::new();
    let preview_task = tasks.spawn(async move {
        preview
            .command(&invocation(9820, Some(true), &["duration"]))
            .await;
    });
    let arrived = tokio::time::timeout(BOUND, gate.wait_for_request())
        .await
        .is_ok();
    if !arrived {
        gate.release();
        tasks.shutdown().await;
        preview_pool.close().await;
        preview_mock.shutdown().await;
        panic!("fixture startup: preview did not reach the private REST response barrier");
    }
    let preview_held_before = !preview_task.is_finished();
    let (live_pool, live_listener) = tripwire().await;
    let live_mock = rest().await;
    let (live, _) = runtime(&live_pool, &live_mock, ordinary_now());
    tasks.spawn(async move {
        live.command(&invocation(9821, Some(false), &["duration"]))
            .await;
    });
    // Live SQL must start while the preview's HTTP response is still withheld.
    // An uncompleted preview must not retain EFFECTS across private transport I/O.
    let acquired = tokio::time::timeout(BOUND, live_listener.accept()).await;
    let reached_sql = matches!(&acquired, Ok(Ok(_)));
    let preview_held_after = !preview_task.is_finished();
    let requests = preview_mock.requests();
    let live_requests = live_mock.requests();
    gate.release();
    let response_released = tokio::time::timeout(BOUND, gate.wait_for_completion())
        .await
        .is_ok();
    tasks.shutdown().await;
    drop(acquired);
    drop(live_listener);
    live_pool.close().await;
    preview_pool.close().await;
    live_mock.shutdown().await;
    preview_mock.shutdown().await;
    assert!(
        preview_held_before && preview_held_after,
        "preview did not stay behind the response barrier"
    );
    assert!(
        response_released,
        "fixture failed to release the held response"
    );
    no_sql(preview_listener);
    assert_eq!(nonce_calls.load(Ordering::SeqCst), 0);
    assert_preview(&requests);
    assert!(
        live_requests.is_empty(),
        "live create cannot publish before SQL completes"
    );
    assert!(
        reached_sql,
        "held private preview response retained the live-effect lock"
    );
}

#[tokio::test]
async fn duration_alias_collision_refuses_in_both_orders_and_create_modes() {
    for mode in [Some(true), Some(false), None] {
        for names in [
            ["duration", "duration-minutes"],
            ["duration-minutes", "duration"],
        ] {
            let (pool, listener) = tripwire().await;
            let mock = rest().await;
            let (runtime, nonce_calls) = runtime(&pool, &mock, ordinary_now());
            let interaction = invocation(9830, mode, &names);
            let completed = tokio::time::timeout(BOUND, runtime.command(&interaction))
                .await
                .is_ok();
            let requests = mock.requests();
            pool.close().await;
            mock.shutdown().await;
            no_sql(listener);
            assert!(completed, "alias refusal blocked: {mode:?} {names:?}");
            assert_eq!(
                nonce_calls.load(Ordering::SeqCst),
                0,
                "alias refusal minted randomness: {mode:?} {names:?}"
            );
            assert_refusal(&requests);
        }
    }
}

#[tokio::test]
async fn checked_deadline_overflow_refuses_in_preview_and_real_create() {
    // Date::MAX exists in the pinned time crate and follows its large-dates
    // feature. Build a supported instant, rather than guessing an OffsetDateTime
    // MAX constant or injecting an already-invalid timestamp.
    let near_max = time::Date::MAX
        .with_hms(23, 59, 30)
        .expect("valid final minute")
        .assume_utc();
    assert!(
        near_max.checked_add(time::Duration::minutes(1)).is_none(),
        "fixture must overflow only when adding a valid duration"
    );
    for mode in [Some(true), Some(false), None] {
        let (pool, listener) = tripwire().await;
        let mock = rest().await;
        let (runtime, nonce_calls) = runtime(&pool, &mock, near_max);
        let interaction = invocation(9840, mode, &["duration"]);
        let completed = tokio::time::timeout(BOUND, runtime.command(&interaction))
            .await
            .is_ok();
        let requests = mock.requests();
        pool.close().await;
        mock.shutdown().await;
        no_sql(listener);
        assert!(completed, "deadline-overflow refusal blocked: {mode:?}");
        assert_eq!(
            nonce_calls.load(Ordering::SeqCst),
            0,
            "overflow refusal minted randomness: {mode:?}"
        );
        assert_refusal(&requests);
    }
}
