//! Acceptance for the backfill rate-limit client core.
//!
//! Pins the shared backfill REST client: header-driven per-route buckets,
//! shared global pacing across independent executors, `Retry-After`/
//! `X-RateLimit-Reset-After` sleep-then-retry, invalid-budget accounting with
//! the shared-scope exemption, header-absent conservative pacing, and guard
//! pacing turns that stay one global interval apart even when every waiter is
//! polled late. Test files only; no `src` changes. Loopback fixtures only: no
//! database, never staging or production.
//!
//! Timing oracles. The loopback server polls its accepts and reads
//! asynchronously, so its receipt instants can bunch together or drift
//! against the client's synchronous sends: a span of server receipts is no
//! admission oracle. Pacing is judged on the client attempt counter instead.
//! `ActionExecutor::requests` increments synchronously after guard admission,
//! before the request reaches the wire; the delayed-observer test proves it
//! counts attempts, not construction or completion. Every read of the
//! counters is bracketed by the instants just before and just after it, so the
//! attempts counted between two reads happened inside that bracket. A slow
//! observer only widens a bracket and never shrinks one, so the oracle cannot
//! false-fail on a loaded runner. Retry pauses anchor on the instant the server
//! was about to write the 429 (no client can receive it earlier), never on
//! task-spawn time. Server receipt instants stay only where they are sound
//! lower bounds: a request cannot be received before it is sent.
//!
//! Rate-limit facts verified against the official Discord documentation
//! (https://docs.discord.com/developers/topics/rate-limits, 2026-10-12):
//! per-route buckets signalled by `X-RateLimit-Bucket`/`Limit`/`Remaining`/
//! `Reset`/`Reset-After`, 429-only `X-RateLimit-Global`/`Scope`
//! (`user`/`global`/`shared`) with `Retry-After`/`retry_after` waits, bots
//! capped at 50 requests/second globally, and 401/403/429 responses counting
//! toward the 10k-per-10-minute invalid-request cutoff except `shared`-scope
//! errors. Small thresholds and short waits below keep the suite fast; no test
//! approaches the real cutoff, and every number the implementation must honor
//! arrives in a response header, never a literal.

use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::Duration,
};

use tokio::time::Instant;
use two_bot_discord::{
    ratelimit_guard::{GuardConfig, GuardError, RateLimitGuard},
    ActionExecutor,
};

// ── Lean loopback double ──────────────────────────────────────────────
// Minimal scripted HTTP server: replays a response queue (status + headers +
// body) while recording every request's method, path and mock-side arrival
// time, plus the instant each response was about to be written. A gated
// server accepts nothing until the test opens its gate. Everything defined
// here is used below, so no lint waiver is needed.

#[derive(Debug, Clone)]
struct FakeResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl FakeResponse {
    fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.to_string().into_bytes(),
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

#[derive(Debug, Clone)]
struct SeenRequest {
    method: String,
    path: String,
    received_at: Instant,
}

struct FakeRest {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    answered: tokio::sync::watch::Receiver<Vec<Instant>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

/// Holds a gated fake server's accept loop closed until [`Self::open`]. The
/// client side runs normally meanwhile (the kernel completes the handshake
/// and buffers the request), so the server observes nothing yet: a delayed
/// observer, as seen from the test.
struct AcceptGate(tokio::sync::oneshot::Sender<()>);

impl AcceptGate {
    /// Opens the gate and returns the instant it opened: the server receives
    /// nothing earlier.
    fn open(self) -> Instant {
        let opened_at = Instant::now();
        self.0.send(()).expect("gated fake server is waiting");
        opened_at
    }
}

impl FakeRest {
    async fn start(script: Vec<FakeResponse>, default: FakeResponse) -> Self {
        Self::start_inner(script, default, None).await
    }

    async fn start_gated(script: Vec<FakeResponse>, default: FakeResponse) -> (Self, AcceptGate) {
        let (open, gate) = tokio::sync::oneshot::channel();
        let mock = Self::start_inner(script, default, Some(gate)).await;
        (mock, AcceptGate(open))
    }

    async fn start_inner(
        script: Vec<FakeResponse>,
        default: FakeResponse,
        gate: Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake rest");
        let addr = listener.local_addr().expect("fake rest addr");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (answered_tx, answered) = tokio::sync::watch::channel(Vec::new());
        let answered_tx = Arc::new(answered_tx);
        let queue = Arc::new(Mutex::new(VecDeque::from(script)));
        let fallback = Arc::new(default);
        let handle = {
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                if let Some(gate) = gate {
                    // A dropped gate opens too: the server never outlives
                    // the test that holds it.
                    let _ = gate.await;
                }
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let seen = Arc::clone(&seen);
                    let answered = Arc::clone(&answered_tx);
                    let queue = Arc::clone(&queue);
                    let fallback = Arc::clone(&fallback);
                    tokio::spawn(async move {
                        serve_conn(stream, seen, answered, queue, fallback).await;
                    });
                }
            })
        };
        Self {
            addr,
            seen,
            answered,
            handle: Some(handle),
        }
    }

    fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().expect("seen").clone()
    }

    /// Bounded handshake on observed server progress: waits until `n`
    /// responses are written and returns the instant the `n`th was about to
    /// be written. No client receives that response, or starts a pause it
    /// carries, before this instant.
    async fn answered(&self, n: usize) -> Instant {
        let mut answered = self.answered.clone();
        let written = tokio::time::timeout(
            Duration::from_secs(15),
            answered.wait_for(|at| at.len() >= n),
        )
        .await
        .expect("fake server answers within the bound")
        .expect("fake server alive");
        written[n - 1]
    }

    async fn shutdown(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl Drop for FakeRest {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

async fn serve_conn(
    mut stream: tokio::net::TcpStream,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    answered: Arc<tokio::sync::watch::Sender<Vec<Instant>>>,
    queue: Arc<Mutex<VecDeque<FakeResponse>>>,
    fallback: Arc<FakeResponse>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let Ok(n) = stream.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return;
        }
    }
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("").to_owned();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    let content_len = lines
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .filter_map(|(_, value)| value.trim().parse::<usize>().ok())
        .next()
        .unwrap_or(0)
        .min(1024 * 1024);
    let mut body = buf[head_end..].to_vec();
    while body.len() < content_len {
        let Ok(n) = stream.read(&mut chunk).await else {
            break;
        };
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    seen.lock().expect("seen").push(SeenRequest {
        method,
        path,
        received_at: Instant::now(),
    });
    let next = queue
        .lock()
        .expect("queue")
        .pop_front()
        .unwrap_or_else(|| (*fallback).clone());
    let mut head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        next.status,
        reason_phrase(next.status),
        next.body.len(),
    );
    for (name, value) in &next.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let answering_at = Instant::now();
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&next.body).await;
    answered.send_modify(|at| at.push(answering_at));
}

fn executor(origin: &str, guard: &Arc<RateLimitGuard>) -> ActionExecutor {
    ActionExecutor::with_proxy_and_guard(
        "fixture-only-backfill-token".to_owned(),
        Some(origin.to_owned()),
        guard.clone(),
    )
    .expect("fixture executor")
}

fn small_guard(threshold: usize) -> Arc<RateLimitGuard> {
    Arc::new(
        RateLimitGuard::new(GuardConfig {
            invalid_request_threshold: threshold,
            ..GuardConfig::default()
        })
        .expect("fixture guard"),
    )
}

// ── Client attempt oracle ─────────────────────────────────────────────

/// Per observed value of the summed client attempt counters: the start of the
/// last read that still showed it and the end of the first read that showed it.
/// The default single-threaded test runtime runs every increment and every
/// read on one thread, so each read sees exactly the attempts made before it.
#[derive(Debug, Default)]
struct AttemptReads(BTreeMap<u64, (Instant, Instant)>);

impl AttemptReads {
    /// One read of every executor's attempt counter, bracketed by the instants
    /// just before and just after it. Reads arrive in time order.
    fn read(&mut self, executors: &[ActionExecutor]) -> u64 {
        let before = Instant::now();
        let attempts = executors.iter().map(ActionExecutor::requests).sum();
        let after = Instant::now();
        self.0
            .entry(attempts)
            .and_modify(|(last_before, _)| *last_before = before)
            .or_insert((before, after));
        attempts
    }

    /// Bounded handshake on client progress: reads on every scheduler turn
    /// until `target` attempts are counted.
    async fn read_until(&mut self, executors: &[ActionExecutor], target: u64) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while self.read(executors) < target {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("client attempts reach the target within the bound");
    }

    /// Sound pacing oracle. The attempts counted between a read showing `low`
    /// and a later read showing `high` happened inside the bracket from the
    /// start of the earlier read to the end of the later one, so those
    /// `high - low` attempts need `high - low - 1` global 20 ms intervals
    /// (50/second, official docs 2026-10-12) inside it. The bracket from the
    /// last read showing no attempt to the first read showing all `total`
    /// carries the aggregate `floor`. Reads may skip intermediate counts when
    /// several attempts land between two of them; the pairs that were observed
    /// and that aggregate bracket still cover every attempt, so keep both.
    fn assert_paced(&self, total: u64, floor: Duration, burst: &str) {
        let interval = Duration::from_millis(20);
        for (&low, &(low_before, _)) in &self.0 {
            for (&high, &(_, high_after)) in self.0.range(low + 1..) {
                let gaps = u32::try_from(high - low - 1).expect("small attempt count");
                let bracket = high_after.saturating_duration_since(low_before);
                assert!(
                    bracket >= interval * gaps,
                    "{burst}: attempts {} through {high} fit in {bracket:?}",
                    low + 1
                );
            }
        }
        let (none_before, _) = self.0.get(&0).expect("a read before the first attempt");
        let (_, all_after) = self.0.get(&total).expect("a read showing every attempt");
        let span = all_after.saturating_duration_since(*none_before);
        assert!(span >= floor, "{burst}: {total} attempts within {span:?}");
    }
}

// (1) Per-route bucket state comes from the X-RateLimit-* headers, never a
// literal: a 200 that reports Remaining 0 with Reset-After 2 on one bucket
// must hold the next read to that route until the header reset.
#[tokio::test]
async fn per_route_bucket_headers_gate_same_bucket_without_hardcoded_limits() {
    let exhausted = FakeResponse::json(200, serde_json::json!({"ok": true}))
        .header("X-RateLimit-Bucket", "fixture-bucket-1")
        .header("X-RateLimit-Limit", "5")
        .header("X-RateLimit-Remaining", "0")
        .header("X-RateLimit-Reset-After", "2");
    let mock = FakeRest::start(
        vec![exhausted],
        FakeResponse::json(200, serde_json::json!({"ok": true})),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("guard"));
    let exec = executor(&mock.origin(), &guard);
    let path = "/guilds/2222";

    let (first, _) = exec
        .get_json_observed(path)
        .await
        .expect("first read")
        .expect("first read carries evidence");
    assert_eq!(first, serde_json::json!({"ok": true}));
    let (second, _) = tokio::time::timeout(Duration::from_secs(15), exec.get_json_observed(path))
        .await
        .expect("second read terminates")
        .expect("second read")
        .expect("second read carries evidence");
    assert_eq!(second, serde_json::json!({"ok": true}));

    let seen = mock.requests();
    assert_eq!(seen.len(), 2);
    // The executor targets the versioned Discord API path; the test route is
    // the suffix.
    assert!(seen
        .iter()
        .all(|r| r.method == "GET" && r.path.ends_with(path)));
    let gap = seen[1].received_at.duration_since(seen[0].received_at);
    assert!(
        gap >= Duration::from_millis(1900),
        "same-bucket second read skipped the header reset: {gap:?}"
    );
    mock.shutdown().await;
}

// (2) Global 50/second coordination is shared across concurrent workers: six
// independent executors on one guard need at least five 20 ms intervals of
// spread (official docs, 2026-10-12), not a six-wide burst. Judged on the
// client attempt counters, not on asynchronously polled server receipts.
#[tokio::test]
async fn independent_workers_share_global_pacing() {
    let path = "/guilds/2222";
    let mock = FakeRest::start(
        vec![],
        FakeResponse::json(200, serde_json::json!({"ok": true})),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("guard"));
    let executors: Vec<ActionExecutor> = (0..6).map(|_| executor(&mock.origin(), &guard)).collect();
    let mut reads = AttemptReads::default();
    assert_eq!(
        reads.read(&executors),
        0,
        "constructing an executor counted an attempt"
    );
    let mut set = tokio::task::JoinSet::new();
    for exec in &executors {
        let exec = exec.clone();
        set.spawn(async move { exec.get_json_once(path).await });
    }
    reads.read_until(&executors, 6).await;
    let mut ok = 0;
    while let Some(joined) = set.join_next().await {
        let data = joined.expect("worker join").expect("worker read");
        assert_eq!(data, Some(serde_json::json!({"ok": true})));
        ok += 1;
    }
    assert_eq!(ok, 6);
    assert_eq!(reads.read(&executors), 6, "a worker attempted twice");
    let seen = mock.requests();
    assert_eq!(seen.len(), 6);
    assert!(seen
        .iter()
        .all(|r| r.method == "GET" && r.path.ends_with(path)));
    reads.assert_paced(
        6,
        Duration::from_millis(100),
        "independent workers burst past shared global pacing",
    );
    mock.shutdown().await;
}

// (2b) Delayed-observer regression: the server accepts nothing until every
// worker's attempt is counted client-side, so its receipts bunch together
// after the gate opens while the attempts themselves were paced. The client
// attempt oracle must still see the full spread (a server receipt span would
// not), and the counters must lead the wire: they count attempts, not
// executor construction or response completion.
#[tokio::test]
async fn pacing_oracle_survives_a_delayed_server_observer() {
    let path = "/guilds/2222";
    let (mock, gate) = FakeRest::start_gated(
        vec![],
        FakeResponse::json(200, serde_json::json!({"ok": true})),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("guard"));
    let executors: Vec<ActionExecutor> = (0..6).map(|_| executor(&mock.origin(), &guard)).collect();
    let mut reads = AttemptReads::default();
    assert_eq!(
        reads.read(&executors),
        0,
        "constructing an executor counted an attempt"
    );
    let workers: Vec<_> = executors
        .iter()
        .map(|exec| {
            let exec = exec.clone();
            tokio::spawn(async move { exec.get_json_observed(path).await })
        })
        .collect();
    reads.read_until(&executors, 6).await;
    assert!(
        mock.requests().is_empty(),
        "the server observed a request before its gate opened"
    );
    assert!(
        workers.iter().all(|worker| !worker.is_finished()),
        "a worker finished before any response existed"
    );
    let opened_at = gate.open();
    for worker in workers {
        let (data, evidence) = tokio::time::timeout(Duration::from_secs(15), worker)
            .await
            .expect("delayed worker terminates")
            .expect("worker join")
            .expect("worker read")
            .expect("delayed read carries evidence");
        assert_eq!(data, serde_json::json!({"ok": true}));
        assert!(
            two_bot_core::parse_iso_millis(&evidence).is_some(),
            "evidence is not an ISO request start: {evidence}"
        );
    }
    assert_eq!(reads.read(&executors), 6, "a worker attempted twice");
    let seen = mock.requests();
    assert_eq!(seen.len(), 6);
    assert!(seen
        .iter()
        .all(|r| r.method == "GET" && r.path.ends_with(path) && r.received_at >= opened_at));
    reads.assert_paced(
        6,
        Duration::from_millis(100),
        "workers behind a delayed observer burst past shared global pacing",
    );
    mock.shutdown().await;
}

// (3) Retry-After and X-RateLimit-Reset-After both drive sleep-then-retry,
// with no request sent during the pause. Phase A holds the Retry-After path
// with a value the 1 s no-header default cannot satisfy; phase B
// (Reset-After only) holds the reset hint the same way.
#[tokio::test]
async fn reset_after_header_drives_sleep_then_retry_with_no_request_during_pause() {
    assert_retry_waits_out_the_pause(
        FakeResponse::json(429, serde_json::json!({"global": false})).header("Retry-After", "2"),
        Duration::from_secs(2),
        "Retry-After",
        None,
    )
    .await;
    assert_retry_waits_out_the_pause(
        FakeResponse::json(429, serde_json::json!({})).header("X-RateLimit-Reset-After", "2"),
        Duration::from_millis(1900),
        "X-RateLimit-Reset-After",
        None,
    )
    .await;
}

// (3b) Startup-delay regression: the server holds its first accept for longer
// than a fixed 400 ms timer from task spawn after the client's first attempt
// is counted. The pause oracle anchors on the observed first response, so a
// slow first exchange cannot read as a missing first request.
#[tokio::test]
async fn retry_pause_anchors_on_the_first_response_not_task_spawn() {
    assert_retry_waits_out_the_pause(
        FakeResponse::json(429, serde_json::json!({"global": false})).header("Retry-After", "2"),
        Duration::from_secs(2),
        "Retry-After",
        Some(Duration::from_millis(500)),
    )
    .await;
}

/// One observed read whose first response is the 429 `first` and whose retry
/// gets the default 200. With `startup_delay`, the gated server holds its
/// first accept that long after the client's first attempt is counted. No
/// second attempt may be counted, and no second request received, until
/// `floor` after the server was about to write the 429.
async fn assert_retry_waits_out_the_pause(
    first: FakeResponse,
    floor: Duration,
    header: &str,
    startup_delay: Option<Duration>,
) {
    let path = "/guilds/2222";
    let (mock, gate) = FakeRest::start_gated(
        vec![first],
        FakeResponse::json(200, serde_json::json!({"ok": true})),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("guard"));
    let exec = executor(&mock.origin(), &guard);
    assert_eq!(
        exec.requests(),
        0,
        "constructing the executor counted an attempt"
    );
    let spawned_at = Instant::now();
    let mut task = {
        let exec = exec.clone();
        tokio::spawn(async move { exec.get_json_observed(path).await })
    };
    if let Some(delay) = startup_delay {
        AttemptReads::default()
            .read_until(std::slice::from_ref(&exec), 1)
            .await;
        // Injected stimulus, not synchronization: the first request already
        // sits in the socket while the server observes nothing.
        tokio::time::sleep(delay).await;
        assert!(
            mock.requests().is_empty(),
            "the server observed a request before its gate opened"
        );
    }
    gate.open();
    let answered_at = mock.answered(1).await;
    // Read the attempt counter through the pause (the cadence only tightens
    // the bound; a late read can never make an early retry look compliant).
    let mut retry_counted_by = None;
    let joined = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            tokio::select! {
                joined = &mut task => break joined,
                () = tokio::time::sleep(Duration::from_millis(1)) => {
                    if exec.requests() >= 2 {
                        retry_counted_by.get_or_insert_with(Instant::now);
                    }
                }
            }
        }
    })
    .await
    .expect("retry terminates");
    // If the join won the race before any read saw the retry, completion is
    // the (looser, still sound) upper bound on the retry attempt; the 1 ms
    // reads above are the primary oracle.
    let retry_counted_by = retry_counted_by.unwrap_or_else(Instant::now);
    let (data, evidence) = joined
        .expect("task join")
        .expect("read")
        .expect("retry carries evidence");
    assert_eq!(data, serde_json::json!({"ok": true}));
    assert!(
        two_bot_core::parse_iso_millis(&evidence).is_some(),
        "evidence is not an ISO request start: {evidence}"
    );
    assert_eq!(exec.requests(), 2, "one attempt plus one retry");
    let seen = mock.requests();
    assert_eq!(seen.len(), 2);
    assert!(seen
        .iter()
        .all(|r| r.method == "GET" && r.path.ends_with(path)));
    let early = retry_counted_by.saturating_duration_since(answered_at);
    assert!(
        early >= floor,
        "attempt sent during the {header} pause: {early:?} after the 429"
    );
    let resumed = seen[1].received_at.saturating_duration_since(answered_at);
    assert!(
        resumed >= floor,
        "request received during the {header} pause: {resumed:?} after the 429"
    );
    let gap = seen[1].received_at.duration_since(seen[0].received_at);
    assert!(gap >= floor, "retry skipped the {header} wait: {gap:?}");
    if startup_delay.is_some() {
        let first_receipt = seen[0].received_at.duration_since(spawned_at);
        assert!(
            first_receipt > Duration::from_millis(400),
            "injected startup delay did not outlast a 400 ms spawn timer: {first_receipt:?}"
        );
    }
    mock.shutdown().await;
}

// (4) 401/403/429 responses spend the invalid-request budget (small threshold
// keeps this fast; the real cutoff is 10k/10min per the docs, 2026-10-12),
// while a `shared`-scope 429 is not tallied against the bot. A bot-token 401
// additionally latches `token_invalid`, so the 401 probe runs on its own
// guard after the breaker probe. Counters stay low-cardinality: the snapshot
// carries no token, route or body.
#[tokio::test]
async fn shared_scope_429_does_not_spend_invalid_budget_while_others_do() {
    let mock = FakeRest::start(
        vec![
            FakeResponse::status(403),
            FakeResponse::status(403),
            FakeResponse::json(429, serde_json::json!({"retry_after": 0.0, "global": true})),
        ],
        FakeResponse::json(200, serde_json::json!({"ok": true})),
    )
    .await;
    let guard = small_guard(3);
    let exec = executor(&mock.origin(), &guard);
    let path = "/guilds/2222";

    assert!(exec
        .get_json_observed(path)
        .await
        .expect("403 read")
        .is_none());
    assert!(exec
        .get_json_observed(path)
        .await
        .expect("second 403 read")
        .is_none());
    let (recovered, _) = exec
        .get_json_observed(path)
        .await
        .expect("429 read")
        .expect("global 429 retries into the default 200");
    assert_eq!(recovered, serde_json::json!({"ok": true}));
    assert_eq!(guard.snapshot().invalid_requests_total, 3);
    assert!(guard.snapshot().breaker_open);

    let before = mock.requests().len();
    let refused = exec
        .get_json_observed(path)
        .await
        .expect_err("open breaker refuses");
    assert!(
        refused.contains("circuit breaker"),
        "unexpected refusal: {refused}"
    );
    assert_eq!(
        mock.requests().len(),
        before,
        "refused read reached the wire"
    );
    assert_eq!(guard.snapshot().rejected_requests_total, 1);

    let debug = format!("{:?}", guard.snapshot());
    for secret in ["fixture-only-backfill-token", "/guilds/2222"] {
        assert!(
            !debug.contains(secret),
            "guard snapshot carries high-cardinality detail"
        );
    }
    mock.shutdown().await;

    // A bot-token 401 tallies and latches `token_invalid`: later reads refuse
    // without reaching the wire.
    let mock = FakeRest::start(
        vec![FakeResponse::status(401)],
        FakeResponse::json(200, serde_json::json!({"ok": true})),
    )
    .await;
    let guard = small_guard(3);
    let exec = executor(&mock.origin(), &guard);
    assert!(exec
        .get_json_observed(path)
        .await
        .expect("401 read")
        .is_none());
    assert_eq!(guard.snapshot().invalid_requests_total, 1);
    assert!(guard.snapshot().token_invalid);
    let before = mock.requests().len();
    let refused = exec
        .get_json_observed(path)
        .await
        .expect_err("invalid token refuses");
    assert!(
        refused.contains("token_invalid"),
        "unexpected refusal: {refused}"
    );
    assert_eq!(
        mock.requests().len(),
        before,
        "refused read reached the wire"
    );
    assert_eq!(guard.snapshot().rejected_requests_total, 1);
    mock.shutdown().await;

    let mock = FakeRest::start(
        vec![FakeResponse::json(
            429,
            serde_json::json!({"retry_after": 0.0, "global": false}),
        )
        .header("X-RateLimit-Scope", "shared")],
        FakeResponse::json(200, serde_json::json!({"ok": true})),
    )
    .await;
    let guard = small_guard(3);
    let exec = executor(&mock.origin(), &guard);
    let (recovered, _) =
        tokio::time::timeout(Duration::from_secs(15), exec.get_json_observed(path))
            .await
            .expect("shared-scope retry terminates")
            .expect("read")
            .expect("shared-scope 429 retries into the default 200");
    assert_eq!(recovered, serde_json::json!({"ok": true}));
    assert_eq!(
        mock.requests().len(),
        2,
        "shared-scope 429 must still sleep-then-retry, just not tally"
    );
    assert_eq!(
        guard.snapshot().invalid_requests_total,
        0,
        "shared-scope 429 was tallied against the bot"
    );
    assert!(!guard.snapshot().breaker_open);
    mock.shutdown().await;
}

// (5) Header-absent responses fall back to conservative pacing: with zero
// rate-limit headers anywhere, independent workers on the observed lane must
// still spread across at least three 20 ms global intervals (50/second, docs
// 2026-10-12). Any pacing here comes from the fallback, not a header.
#[tokio::test]
async fn header_absent_responses_keep_conservative_spacing() {
    let path = "/guilds/2222";
    let mock = FakeRest::start(
        vec![],
        FakeResponse::json(200, serde_json::json!({"ok": true})),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("guard"));
    let executors: Vec<ActionExecutor> = (0..4).map(|_| executor(&mock.origin(), &guard)).collect();
    let mut reads = AttemptReads::default();
    assert_eq!(
        reads.read(&executors),
        0,
        "constructing an executor counted an attempt"
    );
    let mut set = tokio::task::JoinSet::new();
    for exec in &executors {
        let exec = exec.clone();
        set.spawn(async move { exec.get_json_observed(path).await });
    }
    reads.read_until(&executors, 4).await;
    let mut ok = 0;
    while let Some(joined) = set.join_next().await {
        let data = joined
            .expect("worker join")
            .expect("worker read")
            .expect("header-absent read carries evidence");
        assert_eq!(data.0, serde_json::json!({"ok": true}));
        assert!(
            two_bot_core::parse_iso_millis(&data.1).is_some(),
            "evidence is not an ISO request start: {}",
            data.1
        );
        ok += 1;
    }
    assert_eq!(ok, 4);
    assert_eq!(reads.read(&executors), 4, "a worker attempted twice");
    let seen = mock.requests();
    assert_eq!(seen.len(), 4);
    assert!(seen
        .iter()
        .all(|r| r.method == "GET" && r.path.ends_with(path)));
    reads.assert_paced(
        4,
        Duration::from_millis(60),
        "header-absent workers burst without conservative fallback pacing",
    );
    mock.shutdown().await;
}

// (6) Stale pacing reservations: a pacing turn is spent when an admission
// actually releases, never reserved ahead of it. On the paused clock six
// waiters are polled once (one admission, five waiting), then not again until
// every turn they could have reserved is overdue. That late poll may release
// at most one of them; the rest keep making progress, each one global 20 ms
// interval (50/second, docs 2026-10-12) after the last. Public guard API
// only: no transport, wall clock or scheduler timing.
#[tokio::test(start_paused = true)]
async fn overdue_pacing_waiters_release_one_global_interval_apart() {
    let guard = RateLimitGuard::new(GuardConfig::default()).expect("guard");
    let mut cx = Context::from_waker(Waker::noop());
    let mut waiting: Vec<_> = (0..6)
        .map(|_| Box::pin(guard.admit_route("GET /guilds/2222", false)))
        .collect();
    let mut released = Vec::new();

    poll_waiters(&mut waiting, &mut cx, &mut released);
    assert_eq!(released.len(), 1, "the first pacing turn admits at once");
    assert_eq!(waiting.len(), 5, "later turns wait for the global interval");

    tokio::time::advance(Duration::from_millis(200)).await;
    let before_late_poll = released.len();
    poll_waiters(&mut waiting, &mut cx, &mut released);
    let overdue = released.len() - before_late_poll;
    assert!(
        overdue <= 1,
        "{overdue} overdue waiters released together on one late poll"
    );

    for _ in 0..1_000 {
        if waiting.is_empty() {
            break;
        }
        tokio::time::advance(Duration::from_millis(1)).await;
        poll_waiters(&mut waiting, &mut cx, &mut released);
    }
    assert!(
        waiting.is_empty(),
        "{} paced waiters stopped making progress",
        waiting.len()
    );
    assert_eq!(released.len(), 6);
    for pair in released.windows(2) {
        let gap = pair[1].duration_since(pair[0]);
        assert!(
            gap >= Duration::from_millis(20),
            "admissions released {gap:?} apart, under the 20 ms global interval"
        );
    }
}

/// Polls every waiting admission once, in order, recording the paused-clock
/// instant each one releases at.
fn poll_waiters<F>(
    waiting: &mut Vec<Pin<Box<F>>>,
    cx: &mut Context<'_>,
    released: &mut Vec<Instant>,
) where
    F: Future<Output = Result<(), GuardError>>,
{
    waiting.retain_mut(|turn| match turn.as_mut().poll(cx) {
        Poll::Ready(admitted) => {
            admitted.expect("pacing admission");
            released.push(Instant::now());
            false
        }
        Poll::Pending => true,
    });
}
