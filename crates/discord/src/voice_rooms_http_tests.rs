//! Loopback-only tests of the actual Twilight request builders and transport.

use super::*;
use axum::{
    body::Body,
    extract::State,
    http::{Method, Response, StatusCode, Uri},
    Router,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use tokio::{net::TcpListener, task::JoinHandle};

#[derive(Clone)]
struct ScriptedResponse {
    status: u16,
    body: Value,
    headers: Vec<(&'static str, &'static str)>,
}

fn response(status: u16, body: Value) -> ScriptedResponse {
    ScriptedResponse {
        status,
        body,
        headers: vec![],
    }
}

#[derive(Debug)]
struct RecordedRequest {
    method: Method,
    path: String,
    body: Value,
    at: Instant,
    bot_authenticated: bool,
}

#[derive(Clone)]
struct MockState {
    script: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
}

struct Mock {
    api: RoomHttp,
    state: MockState,
    task: JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Mock {
    async fn start(script: Vec<ScriptedResponse>) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let state = MockState {
            script: Arc::new(Mutex::new(script.into())),
            recorded: Arc::new(Mutex::new(vec![])),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().fallback(handle).with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let api = RoomHttp::with_origin(
            "loopback-only-test-token".to_owned(),
            format!("http://{address}/api/v10"),
            true,
        )
        .unwrap();
        assert!(!format!("{api:?}").contains("loopback-only-test-token"));
        Self { api, state, task }
    }
}

async fn handle(
    State(state): State<MockState>,
    method: Method,
    uri: Uri,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response<Body> {
    state.recorded.lock().unwrap().push(RecordedRequest {
        method,
        path: uri.path().to_owned(),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        at: Instant::now(),
        bot_authenticated: headers.contains_key(AUTHORIZATION),
    });
    let next = state
        .script
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(response(500, json!({"code": 0})));
    let mut reply = Response::builder()
        .status(StatusCode::from_u16(next.status).unwrap())
        .header(CONTENT_TYPE, "application/json");
    for (name, value) in next.headers {
        reply = reply.header(name, value);
    }
    reply
        .body(if next.status == 204 {
            Body::empty()
        } else {
            Body::from(next.body.to_string())
        })
        .unwrap()
}

fn attributes() -> RoomChannelAttributes {
    RoomChannelAttributes {
        parent_id: Some(400),
        bitrate: Some(96000),
        rtc_region: Some("rotterdam".to_owned()),
        video_quality_mode: Some(VideoQualityMode::Full),
        nsfw: true,
        user_limit: 8,
        position: None,
        overwrites: vec![PermissionOverwrite {
            id: Id::new(100),
            kind: PermissionOverwriteType::Role,
            allow: Permissions::VIEW_CHANNEL | Permissions::CONNECT,
            deny: Permissions::empty(),
        }],
    }
}

fn created_channel() -> Value {
    json!({ "id": "600", "guild_id": "100", "type": 2, "name": "room" })
}

fn deferred_response() -> InteractionResponse {
    use twilight_model::{
        channel::message::MessageFlags,
        http::interaction::{InteractionResponseData, InteractionResponseType},
    };
    InteractionResponse {
        kind: InteractionResponseType::DeferredChannelMessageWithSource,
        data: Some(InteractionResponseData {
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    }
}

#[tokio::test]
async fn ephemeral_defer_then_edit_original_use_token_only_and_no_mentions() {
    let mock = Mock::start(vec![response(204, Value::Null), response(200, json!({}))]).await;
    mock.api
        .respond_interaction(
            Id::new(1),
            Id::new(2),
            "interaction-token",
            &deferred_response(),
        )
        .await
        .unwrap();
    mock.api
        .complete_interaction(Id::new(1), "interaction-token", "Created <#600>")
        .await
        .unwrap();
    let requests = mock.state.recorded.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, Method::POST);
    assert_eq!(
        requests[0].path,
        "/api/v10/interactions/2/interaction-token/callback"
    );
    assert_eq!(requests[0].body, json!({"type": 5, "data": {"flags": 64}}));
    assert_eq!(requests[1].method, Method::PATCH);
    assert_eq!(
        requests[1].path,
        "/api/v10/webhooks/1/interaction-token/messages/@original"
    );
    assert_eq!(requests[1].body["content"], "Created <#600>");
    assert_eq!(requests[1].body["allowed_mentions"]["parse"], json!([]));
    assert!(requests.iter().all(|request| !request.bot_authenticated));
}

#[tokio::test]
async fn callback_429_is_single_attempt_and_does_not_set_bot_global_backoff() {
    let mock = Mock::start(vec![
        response(429, json!({"retry_after": 600, "global": true})),
        response(204, Value::Null),
    ])
    .await;
    assert_eq!(
        mock.api
            .respond_interaction(
                Id::new(1),
                Id::new(2),
                "interaction-token",
                &deferred_response()
            )
            .await,
        Err(RoomHttpError::RateLimited {
            retry_after_ms: 600000,
            global: true
        })
    );
    tokio::time::timeout(Duration::from_secs(1), mock.api.delete_room(600, || true))
        .await
        .unwrap()
        .unwrap();
    let requests = mock.state.recorded.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].bot_authenticated);
}

#[tokio::test]
async fn expired_interaction_token_does_not_halt_bot_credentials() {
    let mock = Mock::start(vec![response(401, json!({})), response(204, Value::Null)]).await;
    assert_eq!(
        mock.api
            .respond_interaction(
                Id::new(1),
                Id::new(2),
                "expired-token",
                &deferred_response()
            )
            .await,
        Err(RoomHttpError::Unauthorized)
    );
    mock.api.delete_room(600, || true).await.unwrap();
    assert_eq!(mock.state.recorded.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn callback_bypasses_bot_backoff_and_credential_halt_without_substitution() {
    let mock = Mock::start(vec![response(204, Value::Null)]).await;
    *mock.api.global_not_before.lock().unwrap() = Some(Instant::now() + Duration::from_secs(600));
    mock.api.unauthorized.store(true, Ordering::Relaxed);
    tokio::time::timeout(
        Duration::from_secs(1),
        mock.api.respond_interaction(
            Id::new(1),
            Id::new(2),
            "interaction-token",
            &deferred_response(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!mock.state.recorded.lock().unwrap()[0].bot_authenticated);
    assert_eq!(
        mock.api.delete_room(600, || true).await,
        Err(RoomHttpError::Unauthorized)
    );
}

#[tokio::test]
async fn sends_overwrites_with_create_then_move_and_treats_only_missing_delete_as_success() {
    let mock = Mock::start(vec![
        response(201, created_channel()),
        response(204, Value::Null),
        response(404, json!({"code": 10003})),
        response(403, json!({"code": 50001})),
    ])
    .await;
    let room = mock
        .api
        .create_room(100, "room", &attributes(), || true)
        .await
        .unwrap();
    assert_eq!(room.id.get(), 600);
    mock.api.move_member(100, 300, 600, || true).await.unwrap();
    mock.api.delete_room(600, || true).await.unwrap();
    assert_eq!(
        mock.api.delete_room(601, || true).await,
        Err(RoomHttpError::AccessDenied)
    );
    let requests = mock.state.recorded.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].method, Method::POST);
    assert_eq!(requests[0].path, "/api/v10/guilds/100/channels");
    assert_eq!(
        requests[0].body,
        json!({
            "name": "room", "type": 2, "parent_id": "400", "bitrate": 96000,
            "rtc_region": "rotterdam", "video_quality_mode": 2, "nsfw": true, "user_limit": 8,
            "permission_overwrites": [{ "id": "100", "type": 0,
                "allow": (Permissions::VIEW_CHANNEL | Permissions::CONNECT).bits().to_string(), "deny": "0" }]
        })
    );
    assert_eq!(requests[1].method, Method::PATCH);
    assert_eq!(requests[1].path, "/api/v10/guilds/100/members/300");
    assert_eq!(requests[1].body, json!({"channel_id": "600"}));
    assert_eq!(requests[2].method, Method::DELETE);
}

#[tokio::test]
async fn create_sends_position_and_omits_an_empty_override_list() {
    let mock = Mock::start(vec![response(201, created_channel())]).await;
    let mut synced = attributes();
    synced.position = Some(3);
    synced.overwrites = Vec::new();
    mock.api
        .create_room(100, "room", &synced, || true)
        .await
        .unwrap();
    let requests = mock.state.recorded.lock().unwrap();
    assert_eq!(requests[0].body["position"], json!(3));
    assert!(
        requests[0].body.get("permission_overwrites").is_none(),
        "an empty set must not be sent, so the room syncs to its category"
    );
}

#[tokio::test]
async fn rename_429_returns_to_the_queue_instead_of_delaying_room_deletion() {
    let mock = Mock::start(vec![
        ScriptedResponse {
            status: 429,
            body: json!({"retry_after": 600.125, "global": false}),
            headers: vec![("retry-after", "600.125"), ("x-ratelimit-scope", "shared")],
        },
        response(204, Value::Null),
    ])
    .await;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        mock.api.rename_room(600, "new name"),
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        Err(RoomHttpError::RateLimited {
            retry_after_ms: 600125,
            global: false
        })
    );
    tokio::time::timeout(Duration::from_secs(2), mock.api.delete_room(600, || true))
        .await
        .unwrap()
        .unwrap();
    let requests = mock.state.recorded.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "exactly one rename attempt; no automatic HTTP retries"
    );
    assert_eq!(requests[0].body, json!({"name": "new name"}));
    assert_eq!(requests[1].method, Method::DELETE);
}

#[tokio::test]
async fn global_retry_after_is_shared_across_clones_and_occupancy_is_checked_after_waiting() {
    let mock = Mock::start(vec![
        ScriptedResponse {
            status: 429,
            body: json!({"retry_after": 0.001, "global": false}),
            headers: vec![("retry-after", "0.08"), ("x-ratelimit-scope", "global")],
        },
        response(204, Value::Null),
    ])
    .await;
    let api = mock.api.clone();
    assert_eq!(
        api.rename_room(600, "pending").await,
        Err(RoomHttpError::RateLimited {
            retry_after_ms: 80,
            global: true,
        })
    );
    // A guild-200 operation sees the same global gate, and the predicate is
    // evaluated after waiting, not just when the action was first queued.
    let deadline = Instant::now() + Duration::from_millis(20);
    assert_eq!(
        mock.api
            .move_member(200, 300, 700, move || Instant::now() < deadline)
            .await,
        Err(RoomHttpError::Cancelled)
    );
    api.delete_room(600, || true).await.unwrap();
    let requests = mock.state.recorded.lock().unwrap();
    assert_eq!(requests.len(), 2, "the stale move was never sent");
    assert!(requests[1].at.duration_since(requests[0].at) >= Duration::from_millis(80));
}

#[tokio::test]
async fn uncertain_creation_is_not_retried_and_preflight_can_cancel_every_write() {
    let mock = Mock::start(vec![response(500, json!({"code": 0}))]).await;
    assert_eq!(
        mock.api
            .create_room(100, "room", &attributes(), || true)
            .await,
        Err(RoomHttpError::UnknownOutcome)
    );
    assert_eq!(
        mock.api
            .create_room(100, "room", &attributes(), || false)
            .await,
        Err(RoomHttpError::Cancelled)
    );
    assert_eq!(
        mock.api.move_member(100, 300, 600, || false).await,
        Err(RoomHttpError::Cancelled)
    );
    assert_eq!(
        mock.api.delete_room(600, || false).await,
        Err(RoomHttpError::Cancelled)
    );
    assert_eq!(mock.state.recorded.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn refused_credential_stops_all_clones_without_more_network_requests() {
    let mock = Mock::start(vec![response(401, json!({"code": 0}))]).await;
    let clone = mock.api.clone();
    assert_eq!(
        mock.api.delete_room(600, || true).await,
        Err(RoomHttpError::Unauthorized)
    );
    assert_eq!(
        clone.create_room(100, "room", &attributes(), || true).await,
        Err(RoomHttpError::Unauthorized)
    );
    assert_eq!(mock.state.recorded.lock().unwrap().len(), 1);
}
