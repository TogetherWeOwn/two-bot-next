//! Protected acceptance: EventSub delivery capacity retention and
//! receiver shutdown join.
//!
//! PROTECTED FILE — the paired Builder must not edit it. If a test looks
//! wrong, hand it back to the test author for correction instead of
//! weakening it here.
//!
//! Contract under test (all behavioral and offline: in-memory ledger/sender
//! fakes, loopback HTTP for the bound receiver, no database, no external
//! network):
//!
//! - A delivery child retains its work capacity until it actually completes,
//!   even after its HTTP request times out. Excess work refuses before
//!   another ledger/send task starts.
//! - Started durable claim/send/settle work survives request cancellation,
//!   stays fenced, and never sends twice to meet a timeout.
//! - Receiver shutdown closes new child admission and joins admitted
//!   children within the existing total service shutdown deadline — never a
//!   second full drain. Deadline exhaustion is reported honestly and fences
//!   are preserved.
//! - Terminal delivery outcome metrics are recorded in the owned completion
//!   path exactly once, including late success/failure after an HTTP
//!   timeout.
//!
//! BASELINE (pre-implementation): the capacity, late-metric and shutdown
//! tests fail because the handler releases its permit when the 20 s request
//! budget expires while the spawned delivery continues, terminal metrics
//! are emitted only after the now-cancellable await, and `serve` drains
//! Axum without joining delivery children. The abort, ambiguous-send and
//! admission-closed tests pass on the baseline and pin behavior the
//! implementation must preserve.

use super::*;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use two_bot_core::stream_live_store::{
    ClaimOutcome, LedgerFuture, LiveClaim, LiveDeliveryConfig, LiveLedger, LiveSendError,
    LiveSender, SendFuture,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum GateRow {
    Started,
    Delivered,
}

/// In-memory ledger double: first claim wins, started/delivered rows dedupe,
/// release frees only a started row. Counters observe every transition so
/// the tests prove refused work never starts and settled work lands exactly
/// once. SQL fences are pinned by the core store tests; only the receiver's
/// use of the trait outcomes matters here.
struct GateLedger {
    claims: AtomicUsize,
    starts: AtomicUsize,
    delivered: AtomicUsize,
    releases: AtomicUsize,
    rows: Mutex<HashMap<String, GateRow>>,
}

impl GateLedger {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            claims: AtomicUsize::new(0),
            starts: AtomicUsize::new(0),
            delivered: AtomicUsize::new(0),
            releases: AtomicUsize::new(0),
            rows: Mutex::new(HashMap::new()),
        })
    }
}

impl LiveLedger for GateLedger {
    fn claim<'a>(&'a self, claim: &'a LiveClaim) -> LedgerFuture<'a, ClaimOutcome> {
        Box::pin(async move {
            self.claims.fetch_add(1, Ordering::SeqCst);
            let mut rows = self.rows.lock().unwrap();
            Ok(if rows.contains_key(&claim.dedup_key) {
                ClaimOutcome::Duplicate
            } else {
                rows.insert(claim.dedup_key.clone(), GateRow::Started);
                ClaimOutcome::Claimed(two_bot_core::Secret::new("gate-token".to_owned()))
            })
        })
    }

    fn mark_started<'a>(
        &'a self,
        _dedup_key: &'a str,
        _claim_token: &'a str,
    ) -> LedgerFuture<'a, bool> {
        Box::pin(async move {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        })
    }

    fn mark_delivered<'a>(
        &'a self,
        dedup_key: &'a str,
        _claim_token: &'a str,
        _message_id: &'a str,
    ) -> LedgerFuture<'a, bool> {
        Box::pin(async move {
            self.delivered.fetch_add(1, Ordering::SeqCst);
            self.rows
                .lock()
                .unwrap()
                .insert(dedup_key.to_owned(), GateRow::Delivered);
            Ok(true)
        })
    }

    fn release_started<'a>(
        &'a self,
        dedup_key: &'a str,
        _claim_token: &'a str,
    ) -> LedgerFuture<'a, bool> {
        Box::pin(async move {
            let mut rows = self.rows.lock().unwrap();
            if rows.remove(dedup_key) == Some(GateRow::Started) {
                self.releases.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }
}

/// Barrier sender double: every send blocks on `release` before answering,
/// so tests hold real delivery children past the HTTP budget. Scripted
/// answers run out last; an unscripted send answers success with a distinct
/// id so an unexpected extra send fails a count assertion instead of
/// panicking inside the child.
struct GateSender {
    entered: AtomicUsize,
    completed: AtomicUsize,
    release: Semaphore,
    script: Mutex<VecDeque<Result<String, LiveSendError>>>,
    calls: Mutex<Vec<String>>,
    default_seq: AtomicUsize,
}

impl GateSender {
    fn scripted(answers: Vec<Result<String, LiveSendError>>) -> Arc<Self> {
        Arc::new(Self {
            entered: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            release: Semaphore::new(0),
            script: Mutex::new(answers.into()),
            calls: Mutex::new(Vec::new()),
            default_seq: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl LiveSender for GateSender {
    fn send_live<'a>(&'a self, _channel_id: &'a str, content: &'a str) -> SendFuture<'a> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(content.to_owned());
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.release
                .acquire()
                .await
                .expect("release gate stays open")
                .forget();
            let answer = self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
                let seq = self.default_seq.fetch_add(1, Ordering::SeqCst);
                Ok(format!("unscripted-msg-{seq}"))
            });
            self.completed.fetch_add(1, Ordering::SeqCst);
            answer
        })
    }
}

/// Poll `cond` across scheduler yields (no timers: safe under paused time).
/// Returns whether `cond` held before the bound. The bound turns a stall
/// into a clean assertion failure at the call site, never a hang.
async fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..10_000 {
        if cond() {
            return true;
        }
        tokio::task::yield_now().await;
    }
    cond()
}

/// One `two_bot_eventsub_posts_total` counter from the process-global
/// registry. Every test that reads it holds `RECEIVER_LOG_SERIAL`, and every
/// router-driving test in this module serializes there too, so the delta
/// cannot race a sibling emission.
fn post_count(outcome: &str) -> u64 {
    let exposition = two_bot_core::metrics::global().render(None);
    let prefix = format!("two_bot_eventsub_posts_total{{outcome=\"{outcome}\"}} ");
    exposition
        .lines()
        .find_map(|line| line.strip_prefix(&prefix)?.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Notification body with a distinct go-live instant: the dedup key binds
/// broadcaster + instant, so distinct `seq` values are distinct streams that
/// each claim, send and settle independently.
fn notification_body_for(seq: u32) -> String {
    format!(
        "{{\"subscription\":{{\"id\":\"synthetic-sub-{seq:04}\",\"status\":\"enabled\",\"type\":\"stream.online\",\"version\":\"1\",\"condition\":{{\"broadcaster_user_id\":\"123456\"}},\"transport\":{{\"method\":\"webhook\",\"callback\":\"https://example.com/hooks/twitch\"}},\"created_at\":\"2026-10-13T00:00:00Z\"}},\"event\":{{\"id\":\"synthetic-stream-{seq:04}\",\"broadcaster_user_id\":\"123456\",\"broadcaster_user_login\":\"ourgamenight\",\"broadcaster_user_name\":\"OurGameNight\",\"type\":\"live\",\"started_at\":\"2026-10-13T20:{:02}:{:02}Z\"}}}}",
        (seq / 60) % 60,
        seq % 60,
    )
}

async fn post_stream(app: Router, seq: u32, message_id: &str) -> Response {
    let body = notification_body_for(seq);
    let timestamp = fresh_timestamp();
    let signature = sign(message_id, &timestamp, body.as_bytes());
    app.oneshot(request_with_content_type(
        EVENTSUB_PATH,
        "application/json",
        "POST",
        TestHeaders {
            message_id: Some(message_id),
            timestamp: Some(&timestamp),
            signature: Some(&signature),
            message_type: Some("notification"),
        },
        Body::from(body),
    ))
    .await
    .unwrap()
}

async fn status_and_code(response: Response) -> (StatusCode, String) {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (
        status,
        value["error"]["code"].as_str().unwrap_or("").to_owned(),
    )
}

/// Drive a probe that must refuse promptly: an admitted probe would block in
/// delivery, so bounded polling turns an admission into a failure message
/// instead of a hang.
async fn expect_refused(app: Router, seq: u32, message_id: &str) -> Response {
    let body = notification_body_for(seq);
    let timestamp = fresh_timestamp();
    let signature = sign(message_id, &timestamp, body.as_bytes());
    let mut probe = Box::pin(app.oneshot(request_with_content_type(
        EVENTSUB_PATH,
        "application/json",
        "POST",
        TestHeaders {
            message_id: Some(message_id),
            timestamp: Some(&timestamp),
            signature: Some(&signature),
            message_type: Some("notification"),
        },
        Body::from(body),
    )));
    for _ in 0..1_000 {
        if let Some(response) = futures_util::future::poll_immediate(&mut probe).await {
            return response.unwrap();
        }
        tokio::task::yield_now().await;
    }
    panic!("probe {message_id} was admitted instead of refused: it blocks in delivery");
}

fn gated_deps(ledger: Arc<GateLedger>, sender: Arc<GateSender>) -> DeliveryDeps {
    DeliveryDeps {
        ledger,
        sender,
        config: LiveDeliveryConfig {
            broadcasters: vec!["123456".to_owned()],
            invite_url: "https://discord.gg/example".to_owned(),
            channel_id: "777".to_owned(),
        },
    }
}

fn gated_app(ledger: Arc<GateLedger>, sender: Arc<GateSender>) -> Router {
    router(Arc::new(ReceiverState::with_delivery(
        &config(),
        Some(gated_deps(ledger, sender)),
    )))
}

/// Bound receiver on a retained ephemeral loopback listener. The port is
/// never released between reservation and bind, so a parallel socket cannot
/// take it and fail the bind spuriously.
async fn bind_gated(deps: DeliveryDeps) -> BoundEventsubReceiver {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback bind");
    BoundEventsubReceiver {
        listener,
        state: Arc::new(ReceiverState::with_delivery(&config(), Some(deps))),
    }
}

async fn send_raw_request(
    stream: &mut tokio::net::TcpStream,
    seq: u32,
    message_id: &str,
) -> std::io::Result<()> {
    let body = notification_body_for(seq);
    let timestamp = fresh_timestamp();
    let signature = sign(message_id, &timestamp, body.as_bytes());
    let request = format!(
        "POST /hooks/twitch HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\ntwitch-eventsub-message-id: {message_id}\r\ntwitch-eventsub-message-timestamp: {timestamp}\r\ntwitch-eventsub-message-signature: {signature}\r\ntwitch-eventsub-message-type: notification\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(request.as_bytes()).await
}

async fn connect_and_send(addr: SocketAddr, seq: u32, message_id: &str) -> tokio::net::TcpStream {
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("loopback connect");
    send_raw_request(&mut stream, seq, message_id)
        .await
        .expect("loopback write");
    stream
}

fn find_crlf_pair(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn response_complete(buf: &[u8]) -> bool {
    let Some(end) = find_crlf_pair(buf) else {
        return false;
    };
    let headers = String::from_utf8_lossy(&buf[..end]);
    let Some(len) = headers.lines().find_map(|line| {
        line.to_ascii_lowercase()
            .strip_prefix("content-length:")?
            .trim()
            .parse::<usize>()
            .ok()
    }) else {
        return false;
    };
    buf.len() >= end + 4 + len
}

fn parse_response(buf: &[u8]) -> (u16, Vec<u8>) {
    let text = String::from_utf8_lossy(buf);
    let status = text
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse::<u16>()
        .unwrap_or(0);
    let body = find_crlf_pair(buf)
        .map(|end| buf[end + 4..].to_vec())
        .unwrap_or_default();
    (status, body)
}

fn error_code(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value["error"]["code"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Read one full response while advancing the paused clock: the server side
/// runs on spawned tasks (its timers fire on advance), the client side is
/// polled between advances. Bounded, so a silent server fails instead of
/// hanging.
async fn read_raw_paused(stream: &mut tokio::net::TcpStream, budget: Duration) -> (u16, Vec<u8>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let steps = budget.as_millis() / 100 + 1;
    for _ in 0..steps {
        let read = {
            let future = Box::pin(stream.read(&mut tmp));
            futures_util::future::poll_immediate(future).await
        };
        match read {
            Some(Ok(0)) => break,
            Some(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
            Some(Err(error)) => panic!("client read failed: {error}"),
            None => {}
        }
        if response_complete(&buf) {
            break;
        }
        tokio::time::advance(Duration::from_millis(100)).await;
    }
    assert!(
        response_complete(&buf) || !buf.is_empty(),
        "server must answer within {budget:?}"
    );
    parse_response(&buf)
}

async fn read_raw_realtime(stream: &mut tokio::net::TcpStream) -> (u16, Vec<u8>) {
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut tmp = [0u8; 4096];
        loop {
            let n = stream.read(&mut tmp).await.expect("client read");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if response_complete(&buf) {
                break;
            }
        }
    })
    .await
    .expect("response within 10 s");
    parse_response(&buf)
}

/// Best-effort probe after shutdown latches: the outcome (refused, silent,
/// or answered) is irrelevant — admission is observed through the
/// ledger/sender counters, never the wire. Never hangs: every phase has a
/// real-time cap.
async fn probe_raw(addr: SocketAddr, seq: u32, message_id: &str) {
    let stream =
        match tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(addr))
            .await
        {
            Ok(Ok(stream)) => stream,
            _ => return,
        };
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let mut stream = stream;
        if send_raw_request(&mut stream, seq, message_id)
            .await
            .is_err()
        {
            return;
        }
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf).await;
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn timed_out_deliveries_retain_capacity_and_reject_excess_before_start() {
    let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
    let attempted_before = post_count("attempted");
    let succeeded_before = post_count("succeeded");
    let failed_before = post_count("failed");

    let ledger = GateLedger::new();
    let sender = GateSender::scripted(
        (0..MAX_REQUESTS)
            .map(|i| Ok(format!("msg-{i}")))
            .collect::<Vec<Result<String, LiveSendError>>>(),
    );
    let app = gated_app(Arc::clone(&ledger), Arc::clone(&sender));

    // Fill every slot with a delivery blocked in its send.
    let mut in_flight = Vec::new();
    for i in 0..MAX_REQUESTS {
        let app = app.clone();
        in_flight.push(tokio::spawn(async move {
            let message_id = format!("synthetic-cap-{i:04}");
            post_stream(app, i as u32, &message_id).await
        }));
    }
    assert!(
        wait_until(|| sender.entered.load(Ordering::SeqCst) == MAX_REQUESTS).await,
        "all {MAX_REQUESTS} deliveries must reach the send gate"
    );
    assert_eq!(
        ledger.claims.load(Ordering::SeqCst),
        MAX_REQUESTS,
        "every admitted delivery claims exactly once"
    );
    assert_eq!(
        ledger.starts.load(Ordering::SeqCst),
        MAX_REQUESTS,
        "every admitted delivery commits its start fence before sending"
    );

    // Excess work refuses before another ledger/send task starts.
    let (status, code) =
        status_and_code(expect_refused(app.clone(), 100, "synthetic-cap-probe-1").await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code, "busy");
    assert_eq!(
        ledger.claims.load(Ordering::SeqCst),
        MAX_REQUESTS,
        "refused work must never claim"
    );
    assert_eq!(
        sender.entered.load(Ordering::SeqCst),
        MAX_REQUESTS,
        "refused work must never send"
    );

    // Every in-flight request times out at the HTTP budget...
    tokio::time::advance(REQUEST_TIMEOUT + Duration::from_secs(1)).await;
    for handle in in_flight {
        let (status, code) = status_and_code(handle.await.unwrap()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(code, "upstream_timeout");
    }

    // ...but the timed-out children still hold their capacity: the next
    // probe refuses instead of starting another delivery.
    let (status, code) =
        status_and_code(expect_refused(app.clone(), 101, "synthetic-cap-probe-2").await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code, "busy");
    assert_eq!(
        ledger.claims.load(Ordering::SeqCst),
        MAX_REQUESTS,
        "capacity must be retained until actual child completion"
    );
    assert_eq!(
        sender.entered.load(Ordering::SeqCst),
        MAX_REQUESTS,
        "no new delivery may start while timed-out children hold capacity"
    );

    // Release every child: each settles exactly once...
    sender.release.add_permits(MAX_REQUESTS);
    assert!(
        wait_until(|| sender.completed.load(Ordering::SeqCst) == MAX_REQUESTS).await,
        "every released child must complete"
    );
    assert_eq!(
        sender.calls().len(),
        MAX_REQUESTS,
        "no second send to meet the timeout"
    );
    assert_eq!(
        ledger.delivered.load(Ordering::SeqCst),
        MAX_REQUESTS,
        "every released child must settle"
    );

    // ...and each records exactly one terminal metric, even though its HTTP
    // request timed out long before it completed.
    assert!(
        wait_until(|| post_count("attempted") == attempted_before + MAX_REQUESTS as u64).await,
        "every late success must record its terminal metric exactly once"
    );
    assert_eq!(
        post_count("succeeded"),
        succeeded_before + MAX_REQUESTS as u64
    );
    assert_eq!(post_count("failed"), failed_before);

    // Capacity returns only now: a fresh stream is admitted and posts.
    sender.release.add_permits(1);
    let admitted = post_stream(app, 102, "synthetic-cap-probe-3").await;
    assert_eq!(admitted.status(), StatusCode::OK);
    assert_eq!(
        ledger.claims.load(Ordering::SeqCst),
        MAX_REQUESTS + 1,
        "released capacity must admit new work"
    );
    assert_eq!(sender.calls().len(), MAX_REQUESTS + 1);
}

#[tokio::test(start_paused = true)]
async fn late_success_after_timeout_settles_once_with_one_terminal_metric() {
    let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
    let attempted_before = post_count("attempted");
    let succeeded_before = post_count("succeeded");
    let failed_before = post_count("failed");

    let ledger = GateLedger::new();
    let sender = GateSender::scripted(vec![Ok("msg-late-1".to_owned())]);
    let app = gated_app(Arc::clone(&ledger), Arc::clone(&sender));

    let mut in_flight = Box::pin(post_stream(app.clone(), 200, "synthetic-late-1"));
    let mut response = None;
    for _ in 0..10_000 {
        if let Some(answered) = futures_util::future::poll_immediate(&mut in_flight).await {
            response = Some(answered);
            break;
        }
        if sender.entered.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        sender.entered.load(Ordering::SeqCst),
        1,
        "the delivery must start before the HTTP budget expires"
    );
    assert!(
        response.is_none(),
        "the request must still be in flight before the budget"
    );

    // The HTTP budget expires while the child is blocked in its send.
    tokio::time::advance(REQUEST_TIMEOUT + Duration::from_secs(1)).await;
    for _ in 0..1_000 {
        if let Some(answered) = futures_util::future::poll_immediate(&mut in_flight).await {
            response = Some(answered);
            break;
        }
        tokio::task::yield_now().await;
    }
    let (status, code) =
        status_and_code(response.expect("the timed-out request must answer")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code, "upstream_timeout");
    assert_eq!(
        post_count("attempted"),
        attempted_before,
        "no terminal metric before child completion"
    );

    // The late send lands, settles, and records exactly one terminal metric.
    sender.release.add_permits(1);
    assert!(
        wait_until(|| sender.completed.load(Ordering::SeqCst) == 1).await,
        "the released child must complete"
    );
    assert!(
        wait_until(|| post_count("attempted") == attempted_before + 1).await,
        "the late success must record its terminal metric exactly once"
    );
    assert_eq!(post_count("succeeded"), succeeded_before + 1);
    assert_eq!(post_count("failed"), failed_before);
    assert_eq!(
        sender.calls().len(),
        1,
        "no second send to meet the timeout"
    );
    assert_eq!(
        ledger.delivered.load(Ordering::SeqCst),
        1,
        "the late success must settle"
    );

    // The Twitch retry of the settled stream dedupes without another send.
    sender.release.add_permits(1);
    let retry = post_stream(app, 200, "synthetic-late-2").await;
    assert_eq!(retry.status(), StatusCode::OK);
    assert_eq!(
        sender.calls().len(),
        1,
        "the retry of a settled stream must not send again"
    );
}

#[tokio::test(start_paused = true)]
async fn late_failure_after_timeout_records_one_terminal_failure_and_releases() {
    let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
    let attempted_before = post_count("attempted");
    let succeeded_before = post_count("succeeded");
    let failed_before = post_count("failed");

    let ledger = GateLedger::new();
    let sender = GateSender::scripted(vec![Err(LiveSendError::ProvenNoSend)]);
    let app = gated_app(Arc::clone(&ledger), Arc::clone(&sender));

    let mut in_flight = Box::pin(post_stream(app.clone(), 210, "synthetic-latefail-1"));
    for _ in 0..10_000 {
        if sender.entered.load(Ordering::SeqCst) == 1 {
            break;
        }
        if futures_util::future::poll_immediate(&mut in_flight)
            .await
            .is_some()
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        sender.entered.load(Ordering::SeqCst),
        1,
        "the delivery must start before the HTTP budget expires"
    );

    tokio::time::advance(REQUEST_TIMEOUT + Duration::from_secs(1)).await;
    let mut response = None;
    for _ in 0..1_000 {
        if let Some(answered) = futures_util::future::poll_immediate(&mut in_flight).await {
            response = Some(answered);
            break;
        }
        tokio::task::yield_now().await;
    }
    let (status, code) =
        status_and_code(response.expect("the timed-out request must answer")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code, "upstream_timeout");

    // The late proven-no-send failure releases and records one terminal
    // failure metric.
    sender.release.add_permits(1);
    assert!(
        wait_until(|| sender.completed.load(Ordering::SeqCst) == 1).await,
        "the released child must complete"
    );
    assert!(
        wait_until(|| post_count("attempted") == attempted_before + 1).await,
        "the late failure must record its terminal metric exactly once"
    );
    assert_eq!(post_count("succeeded"), succeeded_before);
    assert_eq!(post_count("failed"), failed_before + 1);
    assert_eq!(
        sender.calls().len(),
        1,
        "no second send to meet the timeout"
    );
    assert_eq!(
        ledger.releases.load(Ordering::SeqCst),
        1,
        "a proven-no-send failure must release the started claim"
    );
    assert_eq!(ledger.delivered.load(Ordering::SeqCst), 0);

    // The released stream can be claimed and posted by its retry.
    sender.release.add_permits(1);
    let retry = post_stream(app, 210, "synthetic-latefail-2").await;
    assert_eq!(retry.status(), StatusCode::OK);
    assert_eq!(
        sender.calls().len(),
        2,
        "the retry of a released stream posts"
    );
    assert_eq!(ledger.delivered.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn aborted_request_still_settles_without_second_send() {
    let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
    let ledger = GateLedger::new();
    let sender = GateSender::scripted(vec![Ok("msg-abort-1".to_owned())]);
    let app = gated_app(Arc::clone(&ledger), Arc::clone(&sender));

    // Abort the HTTP future mid-delivery, as a disconnect would.
    let handle = tokio::spawn(post_stream(app.clone(), 300, "synthetic-abort-1"));
    assert!(
        wait_until(|| sender.entered.load(Ordering::SeqCst) == 1).await,
        "the delivery must start before the abort"
    );
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());

    // The started work still settles, exactly once.
    sender.release.add_permits(1);
    assert!(
        wait_until(|| sender.completed.load(Ordering::SeqCst) == 1).await,
        "the started child must survive the abort and complete"
    );
    assert_eq!(sender.calls().len(), 1, "no second send after the abort");
    assert_eq!(
        ledger.delivered.load(Ordering::SeqCst),
        1,
        "the started child must settle after the abort"
    );

    // Capacity is restored: a fresh stream is admitted immediately.
    sender.release.add_permits(1);
    let next = post_stream(app, 301, "synthetic-abort-2").await;
    assert_eq!(next.status(), StatusCode::OK);
    assert_eq!(
        ledger.claims.load(Ordering::SeqCst),
        2,
        "completed work must free its capacity"
    );
}

#[tokio::test]
async fn ambiguous_send_keeps_claim_fenced_and_retry_dedupes() {
    let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
    let attempted_before = post_count("attempted");
    let succeeded_before = post_count("succeeded");
    let failed_before = post_count("failed");

    let ledger = GateLedger::new();
    let sender = GateSender::scripted(vec![Err(LiveSendError::Ambiguous)]);
    let app = gated_app(Arc::clone(&ledger), Arc::clone(&sender));

    // The post may have landed: the answer is retryable but the started
    // claim stays fenced — it is never released and never re-sent here.
    sender.release.add_permits(1);
    let (status, code) =
        status_and_code(post_stream(app.clone(), 400, "synthetic-ambiguous-1").await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code, "unavailable");
    assert_eq!(sender.calls().len(), 1, "exactly one send attempt");
    assert_eq!(ledger.starts.load(Ordering::SeqCst), 1);
    assert_eq!(
        ledger.releases.load(Ordering::SeqCst),
        0,
        "an ambiguous send must never release the started claim"
    );
    assert_eq!(ledger.delivered.load(Ordering::SeqCst), 0);
    assert_eq!(post_count("attempted"), attempted_before + 1);
    assert_eq!(post_count("succeeded"), succeeded_before);
    assert_eq!(post_count("failed"), failed_before + 1);

    // The retry sees the fenced claim as a duplicate and sends nothing.
    let retry = post_stream(app, 400, "synthetic-ambiguous-2").await;
    assert_eq!(retry.status(), StatusCode::OK);
    assert_eq!(
        sender.calls().len(),
        1,
        "the retry of a fenced claim must not send again"
    );
    assert_eq!(
        ledger.releases.load(Ordering::SeqCst),
        0,
        "the duplicate retry must not release the fenced claim"
    );
    assert_eq!(
        post_count("attempted"),
        attempted_before + 1,
        "the duplicate retry records no post metric"
    );
}

#[tokio::test(start_paused = true)]
async fn receiver_shutdown_joins_admitted_child_before_serve_returns() {
    let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
    let attempted_before = post_count("attempted");
    let succeeded_before = post_count("succeeded");
    let failed_before = post_count("failed");

    let ledger = GateLedger::new();
    let sender = GateSender::scripted(vec![Ok("msg-join-1".to_owned())]);
    let bound = bind_gated(gated_deps(Arc::clone(&ledger), Arc::clone(&sender))).await;
    let addr = bound.listener.local_addr().expect("bound port");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let serve = tokio::spawn(bound.serve(shutdown_rx));

    // One delivery, blocked in its send.
    let mut client = connect_and_send(addr, 500, "synthetic-join-1").await;
    assert!(
        wait_until(|| sender.entered.load(Ordering::SeqCst) == 1).await,
        "the delivery must start before shutdown latches"
    );

    // Expire the request BEFORE shutdown: the 20 s request timeout must not
    // consume the receiver's shared 4.5 s HTTP-drain/child-join budget. Closing
    // the client removes the HTTP drain as a reason for serve to keep waiting.
    let (status, body) =
        read_raw_paused(&mut client, REQUEST_TIMEOUT + Duration::from_secs(5)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE.as_u16());
    assert_eq!(error_code(&body), "upstream_timeout");
    drop(client);

    let shutdown_started = tokio::time::Instant::now();
    shutdown_tx.send_replace(true);
    // Yield without advancing time so shutdown can latch and HTTP can drain.
    // Serve must then wait for the child, not just the cancelled HTTP owner.
    for _ in 0..1_000 {
        assert!(
            !serve.is_finished(),
            "serve returned while the admitted child was still blocked"
        );
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..1_000 {
        assert!(
            !serve.is_finished(),
            "serve returned before settlement inside the shutdown budget"
        );
        tokio::task::yield_now().await;
    }
    assert_eq!(sender.completed.load(Ordering::SeqCst), 0);
    assert_eq!(ledger.delivered.load(Ordering::SeqCst), 0);
    assert_eq!(ledger.releases.load(Ordering::SeqCst), 0);

    // Release inside the SAME shutdown budget: it settles, records its terminal
    // metric, and only then does serve return. No additional drain is allowed.
    sender.release.add_permits(1);
    assert!(
        wait_until(|| serve.is_finished()).await,
        "serve must return once the admitted child settles"
    );
    let result = serve.await.expect("serve task");
    assert!(
        result.is_ok(),
        "a joined shutdown must succeed, got {result:?}"
    );
    assert!(
        shutdown_started.elapsed() < Duration::from_millis(4_500),
        "valid completion must join within the shared receiver shutdown budget"
    );
    assert_eq!(sender.completed.load(Ordering::SeqCst), 1);
    assert_eq!(ledger.releases.load(Ordering::SeqCst), 0);
    assert_eq!(sender.calls().len(), 1, "no second send across shutdown");
    assert_eq!(
        ledger.delivered.load(Ordering::SeqCst),
        1,
        "the joined child must settle"
    );
    assert!(
        wait_until(|| post_count("attempted") == attempted_before + 1).await,
        "the joined child must record its terminal metric exactly once"
    );
    assert_eq!(post_count("succeeded"), succeeded_before + 1);
    assert_eq!(post_count("failed"), failed_before);
}

#[tokio::test(start_paused = true)]
async fn receiver_shutdown_reports_deadline_exhaustion_without_abandoning_fences() {
    let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
    let ledger = GateLedger::new();
    let sender = GateSender::scripted(vec![Ok("msg-exhaust-1".to_owned())]);
    let bound = bind_gated(gated_deps(Arc::clone(&ledger), Arc::clone(&sender))).await;
    let addr = bound.listener.local_addr().expect("bound port");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let serve = tokio::spawn(bound.serve(shutdown_rx));

    // One delivery, blocked in its send; latch shutdown, expire the HTTP
    // budget, and close the client. The child itself is never released.
    let mut client = connect_and_send(addr, 510, "synthetic-exhaust-1").await;
    assert!(
        wait_until(|| sender.entered.load(Ordering::SeqCst) == 1).await,
        "the delivery must start before shutdown latches"
    );
    shutdown_tx.send_replace(true);
    let (status, _) = read_raw_paused(&mut client, REQUEST_TIMEOUT + Duration::from_secs(5)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE.as_u16());
    drop(client);

    // Run the clock well past the total service shutdown deadline: any join
    // budget within that deadline — never a second full drain — fires.
    let deadline = crate::shutdown::deadline();
    for _ in 0..deadline.as_secs() + 30 {
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    assert!(
        wait_until(|| serve.is_finished()).await,
        "serve must stop waiting within the shutdown deadline, not join forever"
    );
    let result = serve.await.expect("serve task");
    assert!(
        result.is_err(),
        "deadline exhaustion must be reported as an error, not success"
    );

    // The uncompleted child was neither aborted nor released: one send
    // attempt stands, its claim stays fenced, nothing settled.
    assert_eq!(sender.completed.load(Ordering::SeqCst), 0);
    assert_eq!(
        sender.calls().len(),
        1,
        "exactly one send attempt, no retry to meet the deadline"
    );
    assert_eq!(
        ledger.releases.load(Ordering::SeqCst),
        0,
        "deadline exhaustion must not release the started claim"
    );
    assert_eq!(ledger.delivered.load(Ordering::SeqCst), 0);
    assert_eq!(ledger.claims.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn receiver_shutdown_admits_no_new_children() {
    let _receiver_log_serial = RECEIVER_LOG_SERIAL.lock().await;
    let ledger = GateLedger::new();
    let sender = GateSender::scripted(vec![Ok("msg-admit-1".to_owned())]);
    let bound = bind_gated(gated_deps(Arc::clone(&ledger), Arc::clone(&sender))).await;
    let addr = bound.listener.local_addr().expect("bound port");
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let serve = tokio::spawn(bound.serve(shutdown_rx));

    // One delivery, blocked in its send; latch shutdown and let the
    // listener stop admitting.
    let mut first = connect_and_send(addr, 600, "synthetic-admit-1").await;
    assert!(
        wait_until(|| sender.entered.load(Ordering::SeqCst) == 1).await,
        "the delivery must start before shutdown latches"
    );
    shutdown_tx.send_replace(true);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A fresh stream after the latch must not start another delivery. The
    // wire outcome is irrelevant (refused, silent, or answered): admission
    // is observed through the ledger/sender counters.
    probe_raw(addr, 601, "synthetic-admit-2").await;
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        ledger.claims.load(Ordering::SeqCst),
        1,
        "no new claim may start after shutdown latches"
    );
    assert_eq!(
        sender.entered.load(Ordering::SeqCst),
        1,
        "no new send may start after shutdown latches"
    );

    // Release the first delivery: it answers 200 and serve drains cleanly.
    sender.release.add_permits(1);
    let (status, _) = read_raw_realtime(&mut first).await;
    assert_eq!(status, StatusCode::OK.as_u16());
    drop(first);
    let result = tokio::time::timeout(Duration::from_secs(10), serve)
        .await
        .expect("serve drains")
        .expect("serve task");
    assert!(result.is_ok(), "a joined shutdown must succeed");
    assert_eq!(sender.calls().len(), 1);
    assert_eq!(ledger.delivered.load(Ordering::SeqCst), 1);
}
