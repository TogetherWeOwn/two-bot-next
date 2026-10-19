//! Failed early admission-permit releases warn like the receipt path.
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use std::io::Write;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use twilight_http::{request::Request, routing::Route};
use two_bot_core::send_admission::{
    AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion, SendCooldown,
    TokenKey,
};
use two_bot_discord::executor::HyperTransport;

/// Process-wide text log sink. One subscriber serves the whole test binary: a
/// thread-local default can miss events because callsite interest is cached
/// against the first thread that registers it. Readers only consume bytes
/// written after their own mark, filtered by their fixed site label.
#[derive(Clone, Default)]
struct LogSink(Arc<Mutex<Vec<u8>>>);

impl Write for LogSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn log_sink() -> LogSink {
    static SINK: OnceLock<LogSink> = OnceLock::new();
    SINK.get_or_init(|| {
        let sink = LogSink::default();
        let writer = sink.clone();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .with_writer(move || writer.clone())
                .finish(),
        )
        .expect("install the process-wide log sink once");
        sink
    })
    .clone()
}

fn log_mark() -> usize {
    log_sink().0.lock().unwrap().len()
}

/// Log lines since `mark` carrying the shared completion-failure warn.
fn completion_warns_since(mark: usize) -> Vec<String> {
    let bytes = log_sink().0.lock().unwrap()[mark..].to_vec();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| line.contains("Discord send admission completion failed; lane held"))
        .map(str::to_owned)
        .collect()
}

/// Serialise the log-capturing tests: they share one process-wide sink, so a
/// success test asserting zero warns would otherwise race with the failing
/// tests' warns for the same site.
fn log_serial() -> &'static tokio::sync::Mutex<()> {
    static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    SERIAL.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Admission double whose `complete` always fails with storage-unavailable,
/// like a database fault under the receipt path. A failed completion holds
/// the lane by design: `held` stays set.
#[derive(Debug)]
struct FailingRelease {
    completes: Arc<AtomicU32>,
    cooldowns: Arc<Mutex<Vec<Option<SendCooldown>>>>,
}

impl SendCompletion for FailingRelease {
    fn complete(
        self: Box<Self>,
        cooldown: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        self.completes.fetch_add(1, Ordering::SeqCst);
        self.cooldowns.lock().unwrap().push(cooldown);
        // Failed completion holds the lane by design: the lane's `held` flag
        // is never cleared here.
        Box::pin(async { Err(AdmissionError::Storage) })
    }
}

#[derive(Debug)]
struct FailingLane {
    key: TokenKey,
    held: Arc<Mutex<bool>>,
    completes: Arc<AtomicU32>,
    cooldowns: Arc<Mutex<Vec<Option<SendCooldown>>>>,
}

impl FailingLane {
    fn new(token: &str) -> Self {
        Self {
            key: TokenKey::for_bot_token(token).unwrap(),
            held: Arc::new(Mutex::new(false)),
            completes: Arc::new(AtomicU32::new(0)),
            cooldowns: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl SendAdmission for FailingLane {
    fn token_key(&self) -> &TokenKey {
        &self.key
    }

    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        *self.held.lock().unwrap() = true;
        let completes = Arc::clone(&self.completes);
        let cooldowns = Arc::clone(&self.cooldowns);
        Box::pin(async move {
            Ok(AdmissionPermit::new(Box::new(FailingRelease {
                completes,
                cooldowns,
            })))
        })
    }
}

/// Admission double whose `complete` always succeeds, releasing the lane.
/// Pins the success direction: a warn-on-success regression must fail.
#[derive(Debug)]
struct SucceedingRelease {
    completes: Arc<AtomicU32>,
    cooldowns: Arc<Mutex<Vec<Option<SendCooldown>>>>,
    held: Arc<Mutex<bool>>,
}

impl SendCompletion for SucceedingRelease {
    fn complete(
        self: Box<Self>,
        cooldown: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        self.completes.fetch_add(1, Ordering::SeqCst);
        self.cooldowns.lock().unwrap().push(cooldown);
        *self.held.lock().unwrap() = false;
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug)]
struct SucceedingLane {
    key: TokenKey,
    held: Arc<Mutex<bool>>,
    completes: Arc<AtomicU32>,
    cooldowns: Arc<Mutex<Vec<Option<SendCooldown>>>>,
}

impl SucceedingLane {
    fn new(token: &str) -> Self {
        Self {
            key: TokenKey::for_bot_token(token).unwrap(),
            held: Arc::new(Mutex::new(false)),
            completes: Arc::new(AtomicU32::new(0)),
            cooldowns: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl SendAdmission for SucceedingLane {
    fn token_key(&self) -> &TokenKey {
        &self.key
    }

    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        *self.held.lock().unwrap() = true;
        let completes = Arc::clone(&self.completes);
        let cooldowns = Arc::clone(&self.cooldowns);
        let held = Arc::clone(&self.held);
        Box::pin(async move {
            Ok(AdmissionPermit::new(Box::new(SucceedingRelease {
                completes,
                cooldowns,
                held,
            })))
        })
    }
}

fn get_roles_request() -> Request {
    Request::from_route(&Route::GetGuildRoles { guild_id: 2222 })
}

fn governed_transport(origin: String, lane: Arc<dyn SendAdmission>) -> HyperTransport {
    HyperTransport::with_admission("fixture-token".to_owned(), Some(origin), lane)
        .expect("governed transport builds against loopback")
}

/// A GET whose headers never arrive releases its permit in the header-timeout
/// arm. A storage fault there must warn with the `header_timeout` site while
/// the lane stays held and the caller still sees the header bound.
#[tokio::test]
async fn failed_header_timeout_release_warns_with_its_site() {
    let _serial = log_serial().lock().await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mark = log_mark();
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, serde_json::json!([])).delayed(Duration::from_secs(30)),
    )
    .await;
    let lane = Arc::new(FailingLane::new("fixture-token"));
    let transport = governed_transport(mock.origin(), Arc::clone(&lane) as Arc<dyn SendAdmission>);
    let result = transport
        .send_request(
            &get_roles_request(),
            Some(tokio::time::Instant::now() + Duration::from_millis(600)),
        )
        .await;
    assert_eq!(
        result.map(|_| ()),
        Err("read headers timed out".to_owned()),
        "headers stalled, so the stall must report the header bound"
    );
    assert_eq!(
        lane.completes.load(Ordering::SeqCst),
        1,
        "the header-timed-out attempt releases its permit once"
    );
    assert_eq!(
        lane.cooldowns.lock().unwrap().as_slice(),
        [None],
        "early releases carry no cooldown"
    );
    assert!(
        *lane.held.lock().unwrap(),
        "a failed early completion must keep the lane held"
    );
    let warns = completion_warns_since(mark);
    assert!(
        warns.iter().any(|line| line.contains("header_timeout")),
        "a failed header-timeout release must warn with its site; captured: {warns:?}"
    );
    mock.shutdown().await;
}

/// A GET whose headers arrive but whose body stalls releases its permit in the
/// body-timeout arm. A storage fault there must warn with the `body_timeout`
/// site while the lane stays held and the caller still sees the body bound.
#[tokio::test]
async fn failed_body_timeout_release_warns_with_its_site() {
    let _serial = log_serial().lock().await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mark = log_mark();
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, serde_json::json!([])).delayed_body(Duration::from_secs(30)),
    )
    .await;
    let lane = Arc::new(FailingLane::new("fixture-token"));
    let transport = governed_transport(mock.origin(), Arc::clone(&lane) as Arc<dyn SendAdmission>);
    let started = tokio::time::Instant::now();
    let result = transport
        .send_request(
            &get_roles_request(),
            Some(started + Duration::from_millis(600)),
        )
        .await;
    assert_eq!(
        result.map(|_| ()),
        Err("read body timed out".to_owned()),
        "headers arrived, so the stall must report the body bound"
    );
    assert_eq!(
        lane.completes.load(Ordering::SeqCst),
        1,
        "the body-timed-out attempt releases its permit once"
    );
    assert_eq!(
        lane.cooldowns.lock().unwrap().as_slice(),
        [None],
        "early releases carry no cooldown"
    );
    assert!(
        *lane.held.lock().unwrap(),
        "a failed early completion must keep the lane held"
    );
    let warns = completion_warns_since(mark);
    assert!(
        warns.iter().any(|line| line.contains("body_timeout")),
        "a failed body-timeout release must warn with its site; captured: {warns:?}"
    );
    mock.shutdown().await;
}

/// A GET whose body is truncated on the wire releases its permit in the
/// body-error arm. A storage fault there must warn with the `body_error` site
/// while the lane stays held and the caller still sees unavailability. An
/// inline server drives this: it closes with a byte missing, so the body read
/// errors instead of timing out.
#[tokio::test]
async fn failed_body_error_release_warns_with_its_site() {
    let _serial = log_serial().lock().await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mark = log_mark();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind truncate");
    let origin = format!("http://{}", listener.local_addr().expect("addr"));
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut buf = Vec::with_capacity(1024);
        let mut chunk = [0u8; 1024];
        while let Ok(n) = stream.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let body = b"[]";
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len() + 1,
        );
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(body).await;
        // Drop the socket with a byte missing: the body read errors.
    });
    let lane = Arc::new(FailingLane::new("fixture-token"));
    let transport = governed_transport(origin, Arc::clone(&lane) as Arc<dyn SendAdmission>);
    let result = transport
        .send_request(
            &get_roles_request(),
            Some(tokio::time::Instant::now() + Duration::from_secs(10)),
        )
        .await;
    assert_eq!(
        result.map(|_| ()),
        Err("Discord response body unavailable".to_owned()),
        "a truncated body must report unavailability, not a partial read"
    );
    assert_eq!(
        lane.completes.load(Ordering::SeqCst),
        1,
        "the truncated attempt releases its permit once"
    );
    assert_eq!(
        lane.cooldowns.lock().unwrap().as_slice(),
        [None],
        "early releases carry no cooldown"
    );
    assert!(
        *lane.held.lock().unwrap(),
        "a failed early completion must keep the lane held"
    );
    let warns = completion_warns_since(mark);
    assert!(
        warns.iter().any(|line| line.contains("body_error")),
        "a failed body-error release must warn with its site; captured: {warns:?}"
    );
    handle.abort();
}

/// A GET whose headers never arrive but whose permit completes cleanly must
/// stay silent: no completion warn may carry the `header_timeout` site. This
/// pins the success direction against a warn-on-success regression.
#[tokio::test]
async fn successful_header_timeout_release_stays_silent() {
    let _serial = log_serial().lock().await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mark = log_mark();
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, serde_json::json!([])).delayed(Duration::from_secs(30)),
    )
    .await;
    let lane = Arc::new(SucceedingLane::new("fixture-token"));
    let transport = governed_transport(mock.origin(), Arc::clone(&lane) as Arc<dyn SendAdmission>);
    let result = transport
        .send_request(
            &get_roles_request(),
            Some(tokio::time::Instant::now() + Duration::from_millis(600)),
        )
        .await;
    assert_eq!(
        result.map(|_| ()),
        Err("read headers timed out".to_owned()),
        "headers stalled, so the stall must report the header bound"
    );
    assert_eq!(
        lane.completes.load(Ordering::SeqCst),
        1,
        "the header-timed-out attempt releases its permit once"
    );
    assert_eq!(
        lane.cooldowns.lock().unwrap().as_slice(),
        [None],
        "early releases carry no cooldown"
    );
    assert!(
        !*lane.held.lock().unwrap(),
        "a successful early completion must release the lane"
    );
    let warns = completion_warns_since(mark);
    let site_warns: Vec<_> = warns
        .iter()
        .filter(|line| line.contains("header_timeout"))
        .collect();
    assert!(
        site_warns.is_empty(),
        "a successful header-timeout release must not warn; captured: {warns:?}"
    );
    mock.shutdown().await;
}
