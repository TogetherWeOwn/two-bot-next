#![cfg(feature = "db")]
//! Cancellation at durable admission, on the wire, and during settlement.
//! Real Postgres admission and Hyper transport; only the loopback peer and
//! phase barriers are test doubles. No permit is released by test cleanup.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex, Semaphore},
    task::JoinHandle,
};
use twilight_http::{request::Request, routing::Route};
use two_bot_core::send_admission::{
    AdmissionError, AdmissionFuture, AdmissionPermit, PgSendAdmission, SendAdmission,
    SendCompletion, SendCooldown, TokenKey,
};
use two_bot_discord::{executor::HyperTransport, ratelimit_guard::process_guard, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const TOKEN: &str = "cancelled-read-fixture";
const ATTEMPT: Duration = Duration::from_secs(2);
const OBSERVE: Duration = Duration::from_secs(4);

// The header-consumption handshake reads the process guard. Keep independent
// fixture exchanges from satisfying one another's status observation.
fn serial() -> &'static Mutex<()> {
    static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();
    SERIAL.get_or_init(|| Mutex::new(()))
}

async fn headers_consumed(invalid_before: u64) {
    tokio::time::timeout(OBSERVE, async {
        while process_guard().snapshot().invalid_requests_total == invalid_before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("client consumed the error headers");
}

async fn signal(semaphore: &Semaphore) -> bool {
    match tokio::time::timeout(OBSERVE, semaphore.acquire()).await {
        Ok(Ok(permit)) => {
            permit.forget();
            true
        }
        _ => false,
    }
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("test bootstrap required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("disposable migrated fixture")
}

/// Barriers surround the real durable calls, without replacing their storage
/// semantics. In particular, dropping a permit never performs completion here.
#[derive(Debug)]
struct Probe {
    acquired: Semaphore,
    completing: Semaphore,
    settled: Semaphore,
    admit_exited: Semaphore,
    completion_exited: Semaphore,
    active_admits: AtomicUsize,
    active_completions: AtomicUsize,
    cooldowns: Mutex<Vec<Option<SendCooldown>>>,
    admit_gate: Option<Semaphore>,
    completion_gate: Option<Semaphore>,
}

impl Probe {
    fn new(hold_admit: bool, hold_complete: bool) -> Arc<Self> {
        Arc::new(Self {
            acquired: Semaphore::new(0),
            completing: Semaphore::new(0),
            settled: Semaphore::new(0),
            admit_exited: Semaphore::new(0),
            completion_exited: Semaphore::new(0),
            active_admits: AtomicUsize::new(0),
            active_completions: AtomicUsize::new(0),
            cooldowns: Mutex::new(Vec::new()),
            admit_gate: hold_admit.then(|| Semaphore::new(0)),
            completion_gate: hold_complete.then(|| Semaphore::new(0)),
        })
    }
}

struct Active<'a> {
    count: &'a AtomicUsize,
    exited: &'a Semaphore,
}

impl<'a> Active<'a> {
    fn new(count: &'a AtomicUsize, exited: &'a Semaphore) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self { count, exited }
    }
}

impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
        self.exited.add_permits(1);
    }
}

#[derive(Debug)]
struct ObservedAdmission {
    inner: PgSendAdmission,
    probe: Arc<Probe>,
}

impl SendAdmission for ObservedAdmission {
    fn token_key(&self) -> &TokenKey {
        self.inner.token_key()
    }

    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        Box::pin(async move {
            let _active = Active::new(&self.probe.active_admits, &self.probe.admit_exited);
            let permit = self.inner.admit().await?;
            // The database has committed, but the awaiting caller has not yet
            // received its capability. This models cancellation of a DB reply.
            self.probe.acquired.add_permits(1);
            if let Some(gate) = &self.probe.admit_gate {
                let _barrier = gate.acquire().await.expect("admission barrier open");
            }
            Ok(AdmissionPermit::new(Box::new(ObservedCompletion {
                inner: permit,
                probe: Arc::clone(&self.probe),
            })))
        })
    }
}

struct ObservedCompletion {
    inner: AdmissionPermit,
    probe: Arc<Probe>,
}

impl SendCompletion for ObservedCompletion {
    fn complete(
        self: Box<Self>,
        cooldown: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        Box::pin(async move {
            let _active = Active::new(
                &self.probe.active_completions,
                &self.probe.completion_exited,
            );
            self.probe.cooldowns.lock().await.push(cooldown);
            self.probe.completing.add_permits(1);
            if let Some(gate) = &self.probe.completion_gate {
                let _barrier = gate.acquire().await.expect("completion barrier open");
            }
            self.inner.complete(cooldown).await?;
            self.probe.settled.add_permits(1);
            Ok(())
        })
    }
}

fn lane(db: &TestDatabase, probe: &Arc<Probe>) -> Arc<ObservedAdmission> {
    Arc::new(ObservedAdmission {
        inner: PgSendAdmission::new(db.pool().clone(), TOKEN).expect("fixture admission"),
        probe: Arc::clone(probe),
    })
}

#[derive(Debug)]
struct LaneState {
    in_flight: bool,
    indefinite: bool,
    remaining_ms: i64,
    age_ms: i64,
    generation: i64,
}

async fn state(db: &TestDatabase) -> LaneState {
    let (in_flight, indefinite, remaining_ms, age_ms, generation) = sqlx::query_as(
        "SELECT in_flight, indefinite, \
         hold_until_ms - (extract(epoch FROM clock_timestamp()) * 1000)::bigint, \
         (extract(epoch FROM clock_timestamp()) * 1000)::bigint - in_flight_since_ms, \
         generation FROM public.discord_send_admission",
    )
    .fetch_one(db.pool())
    .await
    .expect("one fixture token lane");
    LaneState {
        in_flight,
        indefinite,
        remaining_ms,
        age_ms,
        generation,
    }
}

/// The first request can be stopped before headers or between headers/body.
/// Subsequent requests return the application identity, as a second bootstrap
/// would. Request bytes are recorded only as method/path, never credentials.
struct Peer {
    origin: String,
    received: Arc<Semaphore>,
    headers_sent: Arc<Semaphore>,
    headers: Arc<Semaphore>,
    body: Arc<Semaphore>,
    requests: Arc<Mutex<Vec<String>>>,
    server: JoinHandle<()>,
}

impl Peer {
    async fn start(
        status: u16,
        extra_headers: &str,
        body: &'static str,
        hold_headers: bool,
        hold_body: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let origin = format!(
            "http://{}",
            listener.local_addr().expect("loopback address")
        );
        let received = Arc::new(Semaphore::new(0));
        let headers_sent = Arc::new(Semaphore::new(0));
        let headers = Arc::new(Semaphore::new(usize::from(!hold_headers)));
        let body_gate = Arc::new(Semaphore::new(usize::from(!hold_body)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let response_headers = format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
            body.len()
        );
        let server = {
            let received = Arc::clone(&received);
            let headers_sent = Arc::clone(&headers_sent);
            let headers = Arc::clone(&headers);
            let body_gate = Arc::clone(&body_gate);
            let requests = Arc::clone(&requests);
            tokio::spawn(async move {
                let mut children = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (mut socket, _) = accepted.expect("loopback accept");
                            let received = Arc::clone(&received);
                            let headers_sent = Arc::clone(&headers_sent);
                            let headers = Arc::clone(&headers);
                            let body_gate = Arc::clone(&body_gate);
                            let requests = Arc::clone(&requests);
                            let response_headers = response_headers.clone();
                            children.spawn(async move {
                                let mut request = Vec::new();
                                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                                    let mut byte = [0; 1];
                                    if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                                        return;
                                    }
                                    request.push(byte[0]);
                                    assert!(request.len() < 16_384, "bounded fixture request");
                                }
                                let first = {
                                    let mut seen = requests.lock().await;
                                    let first = seen.is_empty();
                                    seen.push(String::from_utf8_lossy(&request).lines().next().expect("request line").to_owned());
                                    first
                                };
                                received.add_permits(1);
                                if first {
                                    let _header_barrier = headers.acquire().await.expect("header barrier");
                                    if socket.write_all(response_headers.as_bytes()).await.is_ok() {
                                        headers_sent.add_permits(1);
                                        let _body_barrier = body_gate.acquire().await.expect("body barrier");
                                        let _ = socket.write_all(body.as_bytes()).await;
                                    }
                                } else {
                                    let payload = b"{\"id\":\"1111\"}";
                                    let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", payload.len());
                                    if socket.write_all(head.as_bytes()).await.is_ok() {
                                        let _ = socket.write_all(payload).await;
                                    }
                                }
                            });
                        }
                        joined = children.join_next(), if !children.is_empty() => {
                            joined.expect("connection result").expect("loopback handler");
                        }
                    }
                }
            })
        };
        Self {
            origin,
            received,
            headers_sent,
            headers,
            body: body_gate,
            requests,
            server,
        }
    }

    async fn close(self) {
        self.server.abort();
        assert!(self
            .server
            .await
            .expect_err("server aborted")
            .is_cancelled());
    }
}

fn transport(peer: &Peer, admission: Arc<ObservedAdmission>) -> Arc<HyperTransport> {
    Arc::new(
        HyperTransport::with_admission(TOKEN.into(), Some(peer.origin.clone()), admission)
            .expect("governed loopback transport"),
    )
}

fn get() -> Request {
    Request::from_route(&Route::GetGuildRoles { guild_id: 2222 })
}

fn caller(transport: Arc<HyperTransport>, request: Request) -> (JoinHandle<()>, Arc<AtomicUsize>) {
    let published = Arc::new(AtomicUsize::new(0));
    let publication = Arc::clone(&published);
    let caller = tokio::spawn(async move {
        if transport
            .send_request(&request, Some(tokio::time::Instant::now() + ATTEMPT))
            .await
            .is_ok()
        {
            publication.fetch_add(1, Ordering::SeqCst);
        }
    });
    (caller, published)
}

async fn cancel(caller: JoinHandle<()>, published: &AtomicUsize) {
    caller.abort();
    assert!(caller
        .await
        .expect_err("caller cancelled, not a successful result")
        .is_cancelled());
    assert_eq!(published.load(Ordering::SeqCst), 0, "no result publication");
}

#[tokio::test]
async fn uncancelled_read_settles_and_a_second_executor_uses_the_same_pg_lane() {
    let _serial = serial().lock().await;
    let db = database().await;
    let probe = Probe::new(false, false);
    let admission = lane(&db, &probe);
    let peer = Peer::start(200, "", "[]", false, false).await;
    let wire = transport(&peer, admission.clone());
    let result = wire
        .send_request(&get(), Some(tokio::time::Instant::now() + ATTEMPT))
        .await;
    assert!(result.is_ok(), "uncancelled real exchange");
    assert!(
        signal(&probe.settled).await,
        "uncancelled completion observed"
    );
    let first = state(&db).await;
    let next = ActionExecutor::with_admission(TOKEN.into(), Some(peer.origin.clone()), admission)
        .expect("second executor");
    let boot = tokio::time::timeout(OBSERVE, next.current_application_id()).await;
    let paths = peer.requests.lock().await.clone();
    peer.close().await;
    db.close().await.expect("fixture teardown");
    assert!(!first.in_flight && !first.indefinite, "{first:?}");
    assert!(matches!(boot, Ok(Ok(1111))), "second bootstrap: {boot:?}");
    assert_eq!(paths.len(), 2);
    assert!(paths[1].starts_with("GET /api/v10/applications/@me "));
}

#[tokio::test]
async fn cancelled_on_wire_read_settles_and_a_second_executor_bootstraps_before_stale_lease() {
    let _serial = serial().lock().await;
    let db = database().await;
    let probe = Probe::new(false, false);
    let admission = lane(&db, &probe);
    let peer = Peer::start(200, "", "[]", true, false).await;
    let (read, published) = caller(transport(&peer, admission.clone()), get());
    assert!(
        signal(&peer.received).await,
        "GET reached wire after durable admission"
    );
    let held = state(&db).await;
    assert!(held.in_flight && held.age_ms < 60_000, "{held:?}");
    cancel(read, &published).await;
    let before_reply = state(&db).await;
    peer.headers.add_permits(1);
    let settled = signal(&probe.settled).await;
    let after_reply = state(&db).await;
    // Do not invoke the 65 s bootstrap retry loop if settlement failed.
    let boot = if settled {
        let next =
            ActionExecutor::with_admission(TOKEN.into(), Some(peer.origin.clone()), admission)
                .expect("second executor");
        Some(tokio::time::timeout(OBSERVE, next.current_application_id()).await)
    } else {
        None
    };
    let paths = peer.requests.lock().await.clone();
    peer.close().await;
    db.close().await.expect("fixture teardown");
    assert!(
        before_reply.in_flight,
        "cancellation must not release early: {before_reply:?}"
    );
    assert!(
        settled,
        "cancelled GET never completed real admission: {after_reply:?}"
    );
    assert!(
        !after_reply.in_flight && !after_reply.indefinite,
        "{after_reply:?}"
    );
    assert_eq!(
        after_reply.generation, held.generation,
        "completion, not lease reclaim"
    );
    assert!(after_reply.age_ms < 60_000, "not stale-lease recovery");
    assert!(
        matches!(boot, Some(Ok(Ok(1111)))),
        "second bootstrap: {boot:?}"
    );
    assert_eq!(
        paths.len(),
        2,
        "one cancelled read and one fresh bootstrap, no retry"
    );
    assert_eq!(published.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_after_pg_commit_before_admit_reply_does_not_orphan_the_read() {
    let _serial = serial().lock().await;
    let db = database().await;
    let probe = Probe::new(true, false);
    let peer = Peer::start(200, "", "[]", false, false).await;
    let (read, published) = caller(transport(&peer, lane(&db, &probe)), get());
    assert!(
        signal(&probe.acquired).await,
        "real admit committed before cancellation"
    );
    cancel(read, &published).await;
    probe
        .admit_gate
        .as_ref()
        .expect("admit gate")
        .add_permits(1);
    let settled = signal(&probe.settled).await;
    let after = state(&db).await;
    let requests = peer.requests.lock().await.len();
    peer.close().await;
    db.close().await.expect("fixture teardown");
    assert!(
        settled && !after.in_flight,
        "committed read admission was abandoned: {after:?}"
    );
    assert!(requests <= 1, "no retry on behalf of a cancelled caller");
    assert_eq!(published.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_during_completion_does_not_cancel_the_read_release() {
    let _serial = serial().lock().await;
    let db = database().await;
    let probe = Probe::new(false, true);
    let peer = Peer::start(200, "", "[]", false, false).await;
    let (read, published) = caller(transport(&peer, lane(&db, &probe)), get());
    assert!(
        signal(&probe.completing).await,
        "response reached durable completion"
    );
    cancel(read, &published).await;
    let during = state(&db).await;
    probe
        .completion_gate
        .as_ref()
        .expect("completion gate")
        .add_permits(1);
    let settled = signal(&probe.settled).await;
    let after = state(&db).await;
    peer.close().await;
    db.close().await.expect("fixture teardown");
    assert!(
        during.in_flight,
        "no early drop-release while completion is pending"
    );
    assert!(
        settled && !after.in_flight,
        "read completion was cancelled: {after:?}"
    );
    assert_eq!(published.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelled_body_read_accounts_longer_429_body_cooldown_before_releasing() {
    body_cooldown(true).await;
}

#[tokio::test]
async fn uncancelled_body_read_accounts_longer_429_body_cooldown_before_releasing() {
    body_cooldown(false).await;
}

async fn body_cooldown(cancelled: bool) {
    let _serial = serial().lock().await;
    let db = database().await;
    let probe = Probe::new(false, false);
    let admission = lane(&db, &probe);
    let peer = Peer::start(
        429,
        "Retry-After: 20\r\n",
        "{\"retry_after\":40}",
        false,
        true,
    )
    .await;
    let before = process_guard().snapshot().invalid_requests_total;
    let (read, published) = caller(transport(&peer, admission), get());
    assert!(signal(&peer.headers_sent).await, "429 headers sent");
    // This is a client-side header-accounting handshake, not a delay after the
    // server wrote headers: cancellation therefore lands inside body collection.
    headers_consumed(before).await;
    let uncancelled = if cancelled {
        cancel(read, &published).await;
        None
    } else {
        Some(read)
    };
    let during = state(&db).await;
    peer.body.add_permits(1);
    let settled = signal(&probe.settled).await;
    if let Some(read) = uncancelled {
        tokio::time::timeout(OBSERVE, read)
            .await
            .expect("uncancelled caller bounded")
            .expect("uncancelled caller joined");
    }
    let after = state(&db).await;
    let cooldowns = probe.cooldowns.lock().await.clone();
    let second = PgSendAdmission::new(db.pool().clone(), TOKEN).expect("fresh lane client");
    let blocked = matches!(second.admit().await, Err(AdmissionError::Blocked));
    let requests = peer.requests.lock().await.len();
    peer.close().await;
    db.close().await.expect("fixture teardown");
    assert!(
        during.in_flight,
        "headers alone must not release before body timing"
    );
    assert!(settled, "cancelled 429 body was abandoned: {after:?}");
    assert_eq!(cooldowns, vec![Some(SendCooldown::FiniteMs(40_000))]);
    assert!(
        !after.in_flight && !after.indefinite,
        "completed but cooling: {after:?}"
    );
    assert!(
        after.remaining_ms >= 38_000,
        "body delay must not be lost/capped to header: {after:?}"
    );
    assert!(
        blocked,
        "completion installs cooldown atomically before release"
    );
    assert_eq!(
        requests, 1,
        "one wire attempt, never a retry after cancellation"
    );
    assert_eq!(published.load(Ordering::SeqCst), usize::from(!cancelled));
}

#[tokio::test]
async fn cancelled_mutation_after_possible_send_retains_uncertainty_and_never_retries() {
    let _serial = serial().lock().await;
    let db = database().await;
    let probe = Probe::new(false, false);
    let admission = lane(&db, &probe);
    let peer = Peer::start(200, "", "{\"id\":\"5555\"}", true, false).await;
    let request = Request::builder(&Route::CreateMessage { channel_id: 3333 })
        .body(br#"{"content":"fixture"}"#.to_vec())
        .build()
        .expect("fixture mutation request");
    let (write, published) = caller(transport(&peer, admission), request);
    assert!(
        signal(&peer.received).await,
        "mutation may have reached Discord"
    );
    cancel(write, &published).await;
    peer.headers.add_permits(1);
    assert!(
        signal(&peer.headers_sent).await,
        "peer allowed to finish after cancellation"
    );
    let held = state(&db).await;
    let next = PgSendAdmission::new(db.pool().clone(), TOKEN).expect("fresh lane client");
    let blocked = matches!(next.admit().await, Err(AdmissionError::Blocked));
    let requests = peer.requests.lock().await.len();
    let completions = probe.cooldowns.lock().await.clone();
    peer.close().await;
    db.close().await.expect("fixture teardown");
    assert!(
        held.in_flight && held.age_ms < 60_000,
        "uncertain write must remain held: {held:?}"
    );
    assert!(
        blocked,
        "another sender cannot reuse uncertain write admission"
    );
    assert!(
        completions.is_empty(),
        "no read-style completion of uncertain mutation"
    );
    assert_eq!(requests, 1, "no uncertain-write retry");
    assert_eq!(published.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn generic_permit_drop_still_leaves_durable_admission_occupied() {
    let _serial = serial().lock().await;
    let db = database().await;
    let admission = PgSendAdmission::new(db.pool().clone(), TOKEN).expect("fixture lane");
    drop(admission.admit().await.expect("initial admission"));
    let held = state(&db).await;
    let blocked = matches!(admission.admit().await, Err(AdmissionError::Blocked));
    db.close().await.expect("fixture teardown");
    assert!(
        held.in_flight && blocked,
        "no generic Drop release: {held:?}"
    );
}

#[tokio::test]
async fn cancelled_header_stall_remains_bounded_and_releases_the_read_lane() {
    let _serial = serial().lock().await;
    let db = database().await;
    let probe = Probe::new(false, false);
    let peer = Peer::start(200, "", "[]", true, false).await;
    let (read, published) = caller(transport(&peer, lane(&db, &probe)), get());
    assert!(
        signal(&peer.received).await,
        "GET on wire with headers withheld"
    );
    cancel(read, &published).await;
    // Never release the peer. Only the original attempt deadline can settle it.
    let settled = signal(&probe.settled).await;
    let after = state(&db).await;
    let requests = peer.requests.lock().await.len();
    peer.close().await;
    db.close().await.expect("fixture teardown");
    assert!(
        settled && !after.in_flight,
        "cancelled read did not settle inside its bound: {after:?}"
    );
    assert_eq!(requests, 1, "no detached retry loop");
    assert_eq!(published.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelled_body_stall_remains_bounded_and_releases_the_read_lane() {
    let _serial = serial().lock().await;
    let db = database().await;
    let probe = Probe::new(false, false);
    let peer = Peer::start(403, "", "{}", false, true).await;
    let before = process_guard().snapshot().invalid_requests_total;
    let (read, published) = caller(transport(&peer, lane(&db, &probe)), get());
    assert!(
        signal(&peer.headers_sent).await,
        "headers arrived, body withheld"
    );
    headers_consumed(before).await;
    cancel(read, &published).await;
    // No body is released. Its timeout must still settle a nonmutating read.
    let settled = signal(&probe.settled).await;
    let after = state(&db).await;
    let requests = peer.requests.lock().await.len();
    peer.close().await;
    db.close().await.expect("fixture teardown");
    assert!(
        settled && !after.in_flight,
        "cancelled body owner never settled: {after:?}"
    );
    assert_eq!(requests, 1, "no detached read retry");
    assert_eq!(published.load(Ordering::SeqCst), 0);
}

// Unlike caller(), these exchanges deliberately omit the caller deadline. A
// one-hour None fallback must fail inside the observation budget, not hang CI.
#[tokio::test]
async fn omitted_deadline_header_stall_times_out_and_completes_admission_once() {
    default_deadline_header_stall(false).await;
}

#[tokio::test]
async fn cancelled_omitted_deadline_header_stall_settles_once_without_publication() {
    default_deadline_header_stall(true).await;
}

async fn default_deadline_header_stall(cancelled: bool) {
    let _serial = serial().lock().await;
    let db = database().await;
    let probe = Probe::new(false, false);
    let peer = Peer::start(200, "", "[]", true, false).await;
    let wire = transport(&peer, lane(&db, &probe));
    let published = Arc::new(AtomicUsize::new(0));
    let publication = Arc::clone(&published);
    // Real time throughout: pausing Tokio while doing SQL/socket I/O can
    // auto-advance timers before the request has actually reached the peer.
    let began = std::time::Instant::now();
    let mut read = tokio::spawn(async move {
        let result = wire.send_request(&get(), None).await;
        if result.is_ok() {
            publication.fetch_add(1, Ordering::SeqCst);
        }
        matches!(result, Err(ref detail) if detail == "read headers timed out")
    });
    assert!(signal(&probe.acquired).await, "durable admission committed");
    assert!(
        signal(&peer.received).await,
        "admitted GET reached the peer"
    );
    let held = state(&db).await;
    if cancelled {
        read.abort();
        assert!((&mut read)
            .await
            .expect_err("caller cancellation")
            .is_cancelled());
    }
    // The peer never sends headers during this observation. Both the live
    // caller and a detached cancelled read must settle at the five-second
    // default, well before the durable lane's stale-lease recovery.
    let settled = match tokio::time::timeout(Duration::from_secs(8), probe.settled.acquire()).await
    {
        Ok(Ok(permit)) => {
            permit.forget();
            true
        }
        _ => false,
    };
    let elapsed = began.elapsed();
    let after = state(&db).await;
    let cooldowns = probe.cooldowns.lock().await.clone();
    let requests = peer.requests.lock().await.clone();
    let returned_timeout = if !cancelled && settled {
        Some(
            tokio::time::timeout(OBSERVE, &mut read)
                .await
                .expect("settled read returns promptly")
                .expect("caller joined"),
        )
    } else {
        None
    };
    // Failure cleanup only: release a surviving long-deadline mutant so it
    // cannot leave a detached worker or fixture DB behind. This happens AFTER
    // recording every observation used by the deadline/settlement assertions.
    if !settled {
        peer.headers.add_permits(1);
        let _ = signal(&probe.settled).await;
    }
    if !cancelled && returned_timeout.is_none() {
        read.abort();
        let _ = read.await;
    }
    peer.close().await;
    db.close().await.expect("fixture teardown");
    assert!(held.in_flight && held.age_ms < 60_000, "{held:?}");
    assert!(
        settled,
        "omitted deadline did not settle within eight seconds of wire receipt: {after:?}"
    );
    assert!(
        elapsed >= Duration::from_secs(5),
        "default deadline fired early: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "default deadline/settlement exceeded its bound: {elapsed:?}"
    );
    assert!(!after.in_flight && !after.indefinite, "{after:?}");
    assert_eq!(
        after.generation, held.generation,
        "not stale-lease recovery"
    );
    assert_eq!(cooldowns, vec![None], "exactly one completed admission");
    assert_eq!(requests.len(), 1, "one GET, never a detached retry");
    assert!(requests[0].starts_with("GET /api/v10/guilds/2222/roles "));
    assert_eq!(
        published.load(Ordering::SeqCst),
        0,
        "no success publication"
    );
    if !cancelled {
        assert_eq!(returned_timeout, Some(true), "caller sees Timeout");
    }
}

#[tokio::test]
async fn cancelled_admission_wait_has_no_unbounded_detached_owner() {
    let _serial = serial().lock().await;
    let db = database().await;
    let probe = Probe::new(true, false);
    let peer = Peer::start(200, "", "[]", false, false).await;
    let (read, published) = caller(transport(&peer, lane(&db, &probe)), get());
    assert!(
        signal(&probe.acquired).await,
        "DB commit, admission reply withheld"
    );
    cancel(read, &published).await;
    // The barrier never opens. We cannot prove/release a lost capability; the
    // important bound is that its worker exits, leaving conservative storage.
    let exited = signal(&probe.admit_exited).await;
    let active = probe.active_admits.load(Ordering::SeqCst);
    let held = state(&db).await;
    let requests = peer.requests.lock().await.len();
    peer.close().await;
    db.close().await.expect("fixture teardown");
    assert!(
        exited && active == 0,
        "unbounded admission owner after cancellation"
    );
    assert!(
        held.in_flight,
        "unresolved durable outcome must not be guessed free"
    );
    assert_eq!(
        requests, 0,
        "admission did not resolve, so nothing may reach wire"
    );
    assert_eq!(published.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelled_completion_wait_has_no_unbounded_detached_owner() {
    let _serial = serial().lock().await;
    let db = database().await;
    let probe = Probe::new(false, true);
    let peer = Peer::start(200, "", "[]", false, false).await;
    let (read, published) = caller(transport(&peer, lane(&db, &probe)), get());
    assert!(
        signal(&probe.completing).await,
        "completion started, commit withheld"
    );
    cancel(read, &published).await;
    let exited = signal(&probe.completion_exited).await;
    let active = probe.active_completions.load(Ordering::SeqCst);
    let held = state(&db).await;
    peer.close().await;
    db.close().await.expect("fixture teardown");
    assert!(
        exited && active == 0,
        "unbounded completion owner after cancellation"
    );
    assert!(
        held.in_flight,
        "unconfirmed completion cannot release the lane"
    );
    assert_eq!(published.load(Ordering::SeqCst), 0);
}
