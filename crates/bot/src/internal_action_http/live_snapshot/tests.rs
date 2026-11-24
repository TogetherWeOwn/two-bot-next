use super::*;
use axum::body::Body;
use serde_json::{json, Value};
use time::OffsetDateTime;
use twilight_model::id::{marker::GuildMarker, Id as DiscordId};
use two_bot_core::{internal_actions::sign, realtime_wire::watermark::ReservationError, Secret};
use two_bot_testsupport::TestDatabase;

const GUILD: DiscordId<GuildMarker> = DiscordId::new(10);
const KEY_ID: &str = "snapshot-fixture";
const BODY: &str = r#"{"action":"live.snapshot","guild_id":"10"}"#;
const WRONG_GUILD: &str = r#"{"action":"live.snapshot","guild_id":"11"}"#;
const GUILD_ID: Id = Id(GUILD.into_nonzero());
const CREATE: &str = include_str!("../../../tests/fixtures/live_gateway_guild_create.json");
const VOICE: &str = include_str!("../../../tests/fixtures/live_gateway_voice_state_update.json");

#[derive(Clone)]
struct Watermark(Arc<Mutex<u64>>);

impl SequenceWatermark for Watermark {
    async fn read(&self, guild: Id) -> Result<u64, ReservationError> {
        assert_eq!(guild.0, GUILD.into_nonzero());
        Ok(*self.0.lock().expect("fixture watermark"))
    }

    async fn reserve(&self, guild: Id, expected: u64, next: u64) -> Result<(), ReservationError> {
        assert_eq!(guild.0, GUILD.into_nonzero());
        let mut seq = self.0.lock().expect("fixture watermark");
        assert_eq!(*seq, expected);
        *seq = next;
        Ok(())
    }
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test database");
    let db = TestDatabase::create(&url, &sqlx::migrate::Migrator::DEFAULT)
        .await
        .expect("snapshot nonce database");
    sqlx::raw_sql(include_str!(
        "../../../tests/fixtures/live_snapshot_nonce.sql"
    ))
    .execute(db.pool())
    .await
    .expect("snapshot nonce schema");
    db
}

fn key() -> Secret<Vec<u8>> {
    Secret::new(rand::random::<[u8; 32]>().to_vec())
}

fn credential(secret: &Secret<Vec<u8>>, guild: Id) -> (SigningKey, Id) {
    (
        SigningKey {
            id: KEY_ID.into(),
            secret: secret.clone(),
        },
        guild,
    )
}

fn request(secret: &Secret<Vec<u8>>, key_id: &str, raw: &str, nonce: &str) -> Request {
    let timestamp = (now_ms() / 1000).to_string();
    let signature = sign(secret.expose(), &timestamp, nonce, raw.as_bytes());
    Request::builder()
        .method(Method::POST)
        .uri(ACTIONS_PATH)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-two-key-id", key_id)
        .header("x-two-timestamp", timestamp)
        .header("x-two-nonce", nonce)
        .header("x-two-signature", signature)
        .body(Body::from(raw.to_owned()))
        .expect("signed snapshot request")
}

fn nonce() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

async fn begin(owner: &mut LiveGateway<Watermark>, at: WireTime) -> String {
    owner
        .receive(r#"{"op":0,"t":"READY","d":{}}"#, at, |_| false)
        .await;
    let mut nonce = None;
    owner
        .receive(CREATE, at, |request| {
            assert_eq!(request.d.guild_id, GUILD);
            nonce.clone_from(&request.d.nonce);
            true
        })
        .await;
    nonce.expect("snapshot roster request")
}

async fn complete(owner: &mut LiveGateway<Watermark>, at: WireTime, roster_nonce: &str) {
    let chunk = json!({"op":0,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"10","nonce":roster_nonce,
        "chunk_index":0,"chunk_count":1,"members":[{"user":{"id":"31"},"roles":[],"pending":false}]}});
    owner.receive(&chunk.to_string(), at, |_| false).await;
}

async fn json_body(response: Response) -> Value {
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("bounded snapshot or refusal");
    serde_json::from_slice(&bytes).expect("JSON reply")
}

async fn closed(response: Response, code: ErrorCode) {
    assert_eq!(response.status().as_u16(), code.status());
    let body = json_body(response).await;
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["code"], code.as_str());
    assert!(body.get("topics").is_none());
    assert!(body.get("seq").is_none());
}

#[tokio::test]
async fn signed_snapshot_http_covers_the_committed_cursor_and_withdraws_stale_or_incomplete_state()
{
    let db = database().await;
    let watermark = Watermark(Arc::new(Mutex::new(37)));
    let (live, _receiver) = LiveGateway::for_guild(GUILD, watermark.clone());
    let owner = Arc::new(RwLock::new(live));
    let store = InternalActionStore::new(db.pool().clone());
    let secret = key();
    let endpoint = LiveSnapshotEndpoint::new(
        Arc::clone(&owner),
        store.clone(),
        Some(credential(&secret, GUILD_ID)),
    );
    let read = || request(&secret, KEY_ID, BODY, &nonce());

    let old = WireTime(OffsetDateTime::now_utc() - time::Duration::seconds(31));
    let roster = begin(&mut *owner.write().await, old).await;
    closed(endpoint.handle(read()).await, ErrorCode::DiscordUnavailable).await;
    complete(&mut *owner.write().await, old, &roster).await;
    closed(endpoint.handle(read()).await, ErrorCode::DiscordUnavailable).await;

    let now = WireTime(OffsetDateTime::now_utc());
    let mut live = owner.write().await;
    let roster = begin(&mut live, now).await;
    complete(&mut live, now, &roster).await;
    assert_eq!(watermark.read(GUILD_ID).await.unwrap(), 41);
    let observed = WireTime(OffsetDateTime::now_utc());
    live.receive(VOICE, observed, |_| false).await;
    drop(live);

    let first_nonce = nonce();
    let pull = || request(&secret, KEY_ID, BODY, &first_nonce);
    let response = endpoint.handle(pull()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body.as_object().unwrap().len(), 5);
    assert_eq!(body["v"], 1);
    assert_eq!(body["guild_id"], "10");
    assert_eq!(watermark.read(GUILD_ID).await.unwrap(), 42);
    assert_eq!(body["seq"], 42);
    assert_eq!(body["observed_at"], serde_json::to_value(observed).unwrap());
    let topics = &body["topics"];
    assert_eq!(topics.as_object().unwrap().len(), 6);
    assert_eq!(topics["guild"]["ownerId"], "31");
    assert_eq!(topics["channels"].as_array().unwrap().len(), 3);
    assert_eq!(topics["roles"][0]["id"], "10");
    assert_eq!(topics["members"][0]["userId"], "31");
    assert_eq!(topics["voice"][0]["channelId"], "20");
    assert_eq!(topics["events"], json!([]));
    closed(endpoint.handle(pull()).await, ErrorCode::Replayed).await;

    let other = Id(DiscordId::<GuildMarker>::new(11).into_nonzero());
    let other_endpoint = LiveSnapshotEndpoint::new(owner, store, Some(credential(&secret, other)));
    let wrong = request(&secret, KEY_ID, WRONG_GUILD, &nonce());
    closed(
        other_endpoint.handle(wrong).await,
        ErrorCode::DiscordUnavailable,
    )
    .await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn snapshot_http_refuses_disabled_unauthorized_and_unbounded_requests() {
    let db = database().await;
    let (live, _receiver) = LiveGateway::for_guild(GUILD, Watermark(Arc::new(Mutex::new(0))));
    let owner = Arc::new(RwLock::new(live));
    let store = InternalActionStore::new(db.pool().clone());
    let secret = key();
    let endpoint = LiveSnapshotEndpoint::new(
        Arc::clone(&owner),
        store.clone(),
        Some(credential(&secret, GUILD_ID)),
    );
    let disabled = LiveSnapshotEndpoint::new(Arc::clone(&owner), store, None);
    let signed = |body| request(&secret, KEY_ID, body, &nonce());

    // Any accidental read of the owner would wait for the deadline.
    let _lock = owner.write().await;
    closed(
        disabled.handle(signed(BODY)).await,
        ErrorCode::ActionNotAllowed,
    )
    .await;
    let ordinary = request(&key(), "ordinary-fixture", BODY, &nonce());
    closed(endpoint.handle(ordinary).await, ErrorCode::Unauthorized).await;
    for (body, code) in [
        (WRONG_GUILD, ErrorCode::ActionNotAllowed),
        (r#"{"action":"live.snapshot"}"#, ErrorCode::Malformed),
        (
            r#"{"action":"voice.snapshot","guild_id":"10"}"#,
            ErrorCode::ActionNotAllowed,
        ),
    ] {
        closed(endpoint.handle(signed(body)).await, code).await;
    }

    let mut wrong_target = signed(BODY);
    *wrong_target.uri_mut() = "/internal/actions?extra=1".parse().unwrap();
    assert_eq!(
        endpoint.handle(wrong_target).await.status(),
        StatusCode::NOT_FOUND
    );
    let mut oversized = signed(BODY);
    *oversized.body_mut() = Body::from(vec![b' '; MAX_BODY_BYTES + 1]);
    assert_eq!(
        endpoint.handle(oversized).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    db.close().await.unwrap();
}
