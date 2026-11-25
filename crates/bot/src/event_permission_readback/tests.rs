use super::*;
use axum::{body::Body, http};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Map};
use sha2::Sha256;
use tower::ServiceExt;
use twilight_model::{
    gateway::{event::Event, payload::incoming::GuildCreate},
    guild::Permissions,
};
use two_bot_core::internal_actions::body_hash;

static SECRET: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| hex::encode(rand::random::<[u8; 32]>()));
const BODY: &str = r#"{"guild_id":"100","bot_id":"999"}"#;
const NONCE: &str = "0123456789abcdef0123456789abcdef";

fn fixture(missing_overwrites: bool) -> Arc<SharedCache> {
    let cache = Arc::new(SharedCache::default());
    cache.update(&Event::Ready(serde_json::from_value(json!({
        "v":10, "session_id":"fixture", "resume_gateway_url":"ws://fixture.invalid",
        "application":{"id":"999","flags":0}, "guilds":[],
        "user":{"id":"999","bot":true,"username":"fixture","discriminator":"0","mfa_enabled":false}
    })).expect("ready fixture")));
    let mut guild: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/voice_guild.json"))
            .expect("guild fixture");
    guild["roles"][0]["permissions"] = json!(Permissions::CREATE_EVENTS.bits().to_string());
    guild["channels"][1]["type"] = json!(13);
    guild["channels"][0]["permission_overwrites"] = if missing_overwrites {
        Value::Null
    } else {
        json!([{"id":"999","type":1,"allow":Permissions::MANAGE_EVENTS.bits().to_string(),
            "deny":Permissions::CREATE_EVENTS.bits().to_string()}])
    };
    cache.update(&Event::GuildCreate(Box::new(GuildCreate::Available(
        serde_json::from_value(guild).expect("guild update fixture"),
    ))));
    cache
}

fn app(cache: Arc<SharedCache>, gateway: GatewayState, enabled: bool) -> Router {
    let readback = Readback::from_lookup(Some(100), cache, |name| match name {
        ENABLED => Some(u8::from(enabled).to_string()),
        BOT_ID => Some("999".to_owned()),
        KEY => Some(format!("qualification:{}", SECRET.as_str())),
        _ => None,
    });
    let state = crate::server::SharedState::new(Arc::new(RwLock::new(gateway)), None);
    crate::server::router(state.with_event_permission_readback(readback))
}

fn request(body: &str, path: &str) -> http::Request<Body> {
    let timestamp = time::OffsetDateTime::now_utc().unix_timestamp();
    let hash = body_hash(body.as_bytes());
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).expect("fixture HMAC");
    mac.update(format!("POST\n{path}\n{timestamp}\n{NONCE}\n{hash}").as_bytes());
    http::Request::post(PATH)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-two-key-id", "qualification")
        .header("x-two-timestamp", timestamp)
        .header("x-two-nonce", NONCE)
        .header(
            "x-two-signature",
            format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
        )
        .body(Body::from(body.to_owned()))
        .expect("fixture request")
}

#[tokio::test(start_paused = true)]
async fn authorized_scope_returns_only_aggregate_evidence() {
    let app = app(fixture(false), GatewayState::Connected, true);
    let (reading, started) = tokio::sync::oneshot::channel();
    let mut slow = request(BODY, PATH);
    *slow.body_mut() = Body::from_stream(futures_util::stream::once(async move {
        let _ = reading.send(());
        std::future::pending::<Result<&'static str, std::io::Error>>().await
    }));
    let pending = tokio::spawn(app.clone().oneshot(slow));
    started.await.expect("body read started");
    let response = app
        .clone()
        .oneshot(request(BODY, PATH))
        .await
        .expect("response");
    pending.abort();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let bytes = to_bytes(response.into_body(), 1024).await.expect("body");
    let mut evidence: Map<String, Value> = serde_json::from_slice(&bytes).expect("JSON");
    let stamp = evidence.remove("observed_at_unix_seconds");
    assert!(stamp
        .and_then(|time| time.as_i64())
        .is_some_and(|time| time > 0));
    assert_eq!(
        Value::Object(evidence),
        json!({"v":1,"scope_nonce":NONCE,"permissions":{
            "owner_or_admin":false,"guild_create_events":true,"guild_manage_events":false,
            "voice":{"channels":1,"create_events":0,"manage_events":1,"either_event_permission":1},
            "stage":{"channels":1,"create_events":1,"manage_events":0,"either_event_permission":1}
        }})
    );
    let replay = app.oneshot(request(BODY, PATH)).await.expect("response");
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn invalid_scope_auth_and_incomplete_evidence_refuse() {
    let mut unsigned = request(BODY, PATH);
    unsigned.headers_mut().remove("x-two-signature");
    let connected = GatewayState::Connected;
    let wrong_path = request(BODY, "/internal/actions");
    let wrong_guild = request(r#"{"guild_id":"101","bot_id":"999"}"#, PATH);
    let wrong_bot = request(r#"{"guild_id":"100","bot_id":"998"}"#, PATH);
    for (request, gateway, incomplete, enabled, expected) in [
        (unsigned, connected, false, true, 401),
        (wrong_path, connected, false, true, 401),
        (wrong_guild, connected, false, true, 404),
        (wrong_bot, connected, false, true, 404),
        (request(BODY, PATH), connected, false, false, 404),
        (request(BODY, PATH), connected, true, true, 503),
        (request(BODY, PATH), GatewayState::Armed, false, true, 503),
    ] {
        let response = app(fixture(incomplete), gateway, enabled)
            .oneshot(request)
            .await
            .expect("response");
        assert_eq!(response.status().as_u16(), expected);
        assert!(to_bytes(response.into_body(), 1024)
            .await
            .expect("body")
            .is_empty());
    }
}
