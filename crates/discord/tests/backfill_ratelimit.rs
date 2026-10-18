//! Failing acceptance for the backfill rate-limit client core.
//!
//! Paired failing-tests card for the shared backfill REST client: every test
//! below FAILS against the current executor/guard and passes once the builder
//! extends `ratelimit_guard` plus `executor` with header-driven per-route
//! buckets, shared global pacing, `Retry-After`/`X-RateLimit-Reset-After`
//! sleep-then-retry, invalid-budget accounting with the shared-scope
//! exemption, and header-absent conservative pacing. Test files only; no `src`
//! changes. Loopback fixtures only: no database, never staging or production.
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
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use two_bot_discord::{
    ratelimit_guard::{GuardConfig, RateLimitGuard},
    ActionExecutor,
};

// ── Lean loopback double ──────────────────────────────────────────────
// Minimal scripted HTTP server: replays a response queue (status + headers +
// body) while recording every request's method, path and mock-side arrival
// time. Everything defined here is used below, so no lint waiver is needed.

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
    received_at: tokio::time::Instant,
}

struct FakeRest {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl FakeRest {
    async fn start(script: Vec<FakeResponse>, default: FakeResponse) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake rest");
        let addr = listener.local_addr().expect("fake rest addr");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(script)));
        let fallback = Arc::new(default);
        let handle = {
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let seen = Arc::clone(&seen);
                    let queue = Arc::clone(&queue);
                    let fallback = Arc::clone(&fallback);
                    tokio::spawn(async move {
                        serve_conn(stream, seen, queue, fallback).await;
                    });
                }
            })
        };
        Self {
            addr,
            seen,
            handle: Some(handle),
        }
    }

    fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().expect("seen").clone()
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
        received_at: tokio::time::Instant::now(),
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
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&next.body).await;
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

fn arrivals_span(seen: &[SeenRequest]) -> Duration {
    let (mut min, mut max): (Option<tokio::time::Instant>, Option<tokio::time::Instant>) =
        (None, None);
    for request in seen {
        min = Some(min.map_or(request.received_at, |at| at.min(request.received_at)));
        max = Some(max.map_or(request.received_at, |at| at.max(request.received_at)));
    }
    max.unwrap().duration_since(min.unwrap())
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
// spread (official docs, 2026-10-12), not a six-wide burst.
#[tokio::test]
async fn independent_workers_share_global_pacing() {
    let mock = FakeRest::start(
        vec![],
        FakeResponse::json(200, serde_json::json!({"ok": true})),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("guard"));
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..6 {
        let exec = executor(&mock.origin(), &guard);
        set.spawn(async move { exec.get_json_once("/guilds/2222").await });
    }
    let mut ok = 0;
    while let Some(joined) = set.join_next().await {
        let data = joined.expect("worker join").expect("worker read");
        assert_eq!(data, Some(serde_json::json!({"ok": true})));
        ok += 1;
    }
    assert_eq!(ok, 6);
    let seen = mock.requests();
    assert_eq!(seen.len(), 6);
    assert!(seen.iter().all(|r| r.method == "GET"));
    let span = arrivals_span(&seen);
    assert!(
        span >= Duration::from_millis(100),
        "independent workers burst past shared global pacing: {span:?}"
    );
    mock.shutdown().await;
}

// (3) Retry-After and X-RateLimit-Reset-After both drive sleep-then-retry,
// with no request sent during the pause. Phase A locks the Retry-After path
// the reads already honor; phase B (Reset-After only) fails until the builder
// parses that header instead of falling back to the default wait.
#[tokio::test]
async fn reset_after_header_drives_sleep_then_retry_with_no_request_during_pause() {
    let path = "/guilds/2222";

    {
        let mock = FakeRest::start(
            vec![
                FakeResponse::json(429, serde_json::json!({"global": false}))
                    .header("Retry-After", "1"),
            ],
            FakeResponse::json(200, serde_json::json!({"ok": true})),
        )
        .await;
        let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("guard"));
        let exec = executor(&mock.origin(), &guard);
        let task = tokio::spawn(async move { exec.get_json_observed(path).await });
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            mock.requests().len(),
            1,
            "request sent during the Retry-After pause"
        );
        let (data, _) = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("retry terminates")
            .expect("task join")
            .expect("read")
            .expect("retry carries evidence");
        assert_eq!(data, serde_json::json!({"ok": true}));
        let seen = mock.requests();
        assert_eq!(seen.len(), 2);
        let gap = seen[1].received_at.duration_since(seen[0].received_at);
        assert!(
            gap >= Duration::from_secs(1),
            "retry skipped the Retry-After wait: {gap:?}"
        );
        mock.shutdown().await;
    }

    {
        let mock = FakeRest::start(
            vec![FakeResponse::json(429, serde_json::json!({}))
                .header("X-RateLimit-Reset-After", "2")],
            FakeResponse::json(200, serde_json::json!({"ok": true})),
        )
        .await;
        let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("guard"));
        let exec = executor(&mock.origin(), &guard);
        let task = tokio::spawn(async move { exec.get_json_observed(path).await });
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            mock.requests().len(),
            1,
            "request sent during the Reset-After pause"
        );
        let (data, _) = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("retry terminates")
            .expect("task join")
            .expect("read")
            .expect("retry carries evidence");
        assert_eq!(data, serde_json::json!({"ok": true}));
        let seen = mock.requests();
        assert_eq!(seen.len(), 2);
        let gap = seen[1].received_at.duration_since(seen[0].received_at);
        assert!(
            gap >= Duration::from_millis(1900),
            "retry skipped the X-RateLimit-Reset-After wait: {gap:?}"
        );
        mock.shutdown().await;
    }
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
    let mock = FakeRest::start(
        vec![],
        FakeResponse::json(200, serde_json::json!({"ok": true})),
    )
    .await;
    let guard = Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("guard"));
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let exec = executor(&mock.origin(), &guard);
        set.spawn(async move { exec.get_json_observed("/guilds/2222").await });
    }
    let mut ok = 0;
    while let Some(joined) = set.join_next().await {
        let data = joined
            .expect("worker join")
            .expect("worker read")
            .expect("header-absent read carries evidence");
        assert_eq!(data.0, serde_json::json!({"ok": true}));
        ok += 1;
    }
    assert_eq!(ok, 4);
    let seen = mock.requests();
    assert_eq!(seen.len(), 4);
    assert!(seen.iter().all(|r| r.method == "GET"));
    let span = arrivals_span(&seen);
    assert!(
        span >= Duration::from_millis(60),
        "header-absent workers burst without conservative fallback pacing: {span:?}"
    );
    mock.shutdown().await;
}
