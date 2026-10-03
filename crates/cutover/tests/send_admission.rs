//! Cutover uses governed raw attempts, never Twilight's hidden resends.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use twilight_model::id::Id;
use two_bot_core::send_admission::{AdmissionError, PgSendAdmission, SendAdmission};
use two_bot_cutover::{RestClient, RestError};
use two_bot_testsupport::TestDatabase;

struct MockDiscord {
    origin: String,
    count: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl MockDiscord {
    async fn start(status: u16, body: &'static str) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let read = socket.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    assert!(bytes.len() < 64 * 1024);
                    if bytes.windows(4).any(|p| p == b"\r\n\r\n") {
                        break;
                    }
                }
                assert!(bytes.starts_with(b"GET /api/v10/guilds/111111111111111111/channels "));
                seen.fetch_add(1, Ordering::Relaxed);
                let reply = format!("HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        Self {
            origin,
            count,
            task,
        }
    }
}

impl Drop for MockDiscord {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test database URL required");
    TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn cutover_429_retains_shared_indefinite_hold_without_hidden_resend() {
    let db = database().await;
    let second = db.independent_pool().await.unwrap();
    let mock = MockDiscord::start(429, r#"{"global":false}"#).await;
    let gate = Arc::new(PgSendAdmission::new(db.pool().clone(), "cutover-fixture").unwrap());
    let rest = RestClient::with_admission(
        "cutover-fixture".to_owned(),
        Some(mock.origin.clone()),
        gate,
    );
    let outcome = tokio::time::timeout(
        Duration::from_secs(3),
        rest.guild_channels(Id::new(111111111111111111)),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, Err(RestError::Wire(_))));
    let restarted = PgSendAdmission::new(second.clone(), "Bot cutover-fixture").unwrap();
    assert!(matches!(
        restarted.admit().await,
        Err(AdmissionError::Blocked)
    ));
    assert_eq!(mock.count.load(Ordering::Relaxed), 1);
    second.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn cutover_success_releases_lane_and_live_unguarded_constructor_refuses() {
    let db = database().await;
    let mock = MockDiscord::start(200, "[]").await;
    let gate = Arc::new(PgSendAdmission::new(db.pool().clone(), "cutover-fixture").unwrap());
    let rest = RestClient::with_admission(
        "cutover-fixture".to_owned(),
        Some(mock.origin.clone()),
        gate.clone(),
    );
    assert!(rest
        .guild_channels(Id::new(111111111111111111))
        .await
        .unwrap()
        .unwrap()
        .is_empty());
    gate.admit().await.unwrap().complete(None).await.unwrap();
    // This production-shaped constructor must refuse before any wire I/O.
    let unguarded = RestClient::new("cutover-fixture".to_owned());
    assert!(matches!(
        unguarded.guild_channels(Id::new(111111111111111111)).await,
        Err(RestError::Wire(_))
    ));
    assert_eq!(mock.count.load(Ordering::Relaxed), 1);
    db.close().await.unwrap();
}
