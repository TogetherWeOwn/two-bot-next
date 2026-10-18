//! Acceptance: backfill reads wait out a long per-route `Retry-After`
//! instead of retry-storming past it.
//!
//! The legacy 429 wait clamps one wait at 60 s. That clamp is right for the
//! interactive kick/publish lanes, but a backfill read that wakes at 60 s
//! into a 75 s per-route window just earns another 429, spends another of
//! its five tries and another invalid-request tally, and can exhaust its
//! budget against a route that only ever asked it to wait. Backfill read
//! paths must therefore honor the header unclamped, while kick and publish
//! keep the 60 s clamp exactly as today.
//!
//! RED on the baseline: the two long-wait read tests below (75 s header,
//! one wait of at least 75 s on the paused clock, exactly one tally,
//! `Some(ok)`). The short-wait control, the kick/publish clamp pins, and
//! the five-try / invalid-budget exhaustion pin all pass before and after
//! the fix — they are the guardrails the fix must not move.
//!
//! Loopback fixtures only, paused clock: no live Discord, no database,
//! and no 75 s of wall time. Mock-side arrival times ride the same tokio
//! clock the executor sleeps on, so the gap between the two wire requests
//! is the wait the executor actually took. Tests only; no `src` changes.

use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use two_bot_core::KickOutcome;
use two_bot_discord::{
    ratelimit_guard::{GuardConfig, RateLimitGuard},
    ActionExecutor,
};

// ── Lean loopback double ──────────────────────────────────────────────
// Minimal scripted HTTP server (the `backfill_ratelimit` shape): replays a
// response queue (status + headers + body) while recording every request's
// method, path and mock-side arrival time. Everything defined here is used
// below, so no lint waiver is needed.

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
        429 => "Too Many Requests",
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

// ── Fixture helpers ───────────────────────────────────────────────────

const PATH: &str = "/guilds/2222";

fn executor(mock: &FakeRest, guard: &Arc<RateLimitGuard>) -> ActionExecutor {
    ActionExecutor::with_proxy_and_guard(
        "fixture-only-retry-after-token".to_owned(),
        Some(mock.origin()),
        guard.clone(),
    )
    .expect("fixture executor")
}

fn default_guard() -> Arc<RateLimitGuard> {
    Arc::new(RateLimitGuard::new(GuardConfig::default()).expect("fixture guard"))
}

/// A per-route (non-global) 429 whose only wait signal is the
/// `Retry-After` header: the body carries no `retry_after`, so nothing
/// outranks the header and no bucket/scope header installs a guard-side
/// deadline — the executor's own wait is the whole story.
fn rate_limited_header_only(secs: &str) -> FakeResponse {
    FakeResponse::json(429, serde_json::json!({"global": false})).header("Retry-After", secs)
}

fn ok() -> FakeResponse {
    FakeResponse::json(200, serde_json::json!({"ok": true}))
}

/// Mock-side gap between the first two wire requests.
fn first_gap(mock: &FakeRest) -> Duration {
    let seen = mock.requests();
    assert_eq!(seen.len(), 2, "exactly one retry reaches the wire");
    seen[1].received_at.duration_since(seen[0].received_at)
}

// (1) RED: the observed backfill read (the read the store backfill drives)
// waits out a 75 s header in ONE wait. On the baseline the 60 s clamp cuts
// the wait short; against a real route the early retry would 429 again and
// spend a second tally and try. Exactly one tally and `Some(ok)` land here
// either way — the wait length is the behavior under test.
#[tokio::test(start_paused = true)]
async fn observed_read_waits_out_long_retry_after_unclamped() {
    let mock = FakeRest::start(vec![rate_limited_header_only("75")], ok()).await;
    let guard = default_guard();
    let exec = executor(&mock, &guard);

    let (data, _) = tokio::time::timeout(Duration::from_secs(300), exec.get_json_observed(PATH))
        .await
        .expect("read terminates inside the virtual budget")
        .expect("read succeeds")
        .expect("the waited-out retry returns Some(ok)");
    assert_eq!(data, serde_json::json!({"ok": true}));

    let seen = mock.requests();
    assert_eq!(
        seen.len(),
        2,
        "one 429 plus one waited-out retry, never a storm"
    );
    assert!(seen
        .iter()
        .all(|r| r.method == "GET" && r.path.ends_with(PATH)));
    let gap = first_gap(&mock);
    assert!(
        gap >= Duration::from_secs(75),
        "backfill read cut the 75 s Retry-After wait short: {gap:?}"
    );
    assert_eq!(
        guard.snapshot().invalid_requests_total,
        1,
        "exactly one invalid-request tally for the single 429"
    );
    mock.shutdown().await;
}

// (2) RED: the checked read shares the same paced-read policy, so it waits
// out the same 75 s header unclamped and returns `Some(ok)` with one tally.
// Pinning both read loops keeps a half-fix (one loop unclamped, the other
// still storming) from passing.
#[tokio::test(start_paused = true)]
async fn checked_read_waits_out_long_retry_after_unclamped() {
    let mock = FakeRest::start(vec![rate_limited_header_only("75")], ok()).await;
    let guard = default_guard();
    let exec = executor(&mock, &guard);

    let data = tokio::time::timeout(Duration::from_secs(300), exec.get_json_checked(PATH))
        .await
        .expect("read terminates inside the virtual budget")
        .expect("read succeeds")
        .expect("the waited-out retry returns Some(ok)");
    assert_eq!(data, serde_json::json!({"ok": true}));

    let gap = first_gap(&mock);
    assert!(
        gap >= Duration::from_secs(75),
        "checked read cut the 75 s Retry-After wait short: {gap:?}"
    );
    assert_eq!(
        guard.snapshot().invalid_requests_total,
        1,
        "exactly one invalid-request tally for the single 429"
    );
    mock.shutdown().await;
}

// (3) Control (passes before and after): a short 2 s header is honored as
// before — one wait of at least the header, well under the clamp, one
// tally, `Some(ok)`. Proves the harness measures real waits, so the 75 s
// failures above are the clamp and not fixture noise.
#[tokio::test(start_paused = true)]
async fn short_retry_after_control_still_waits_the_header() {
    let mock = FakeRest::start(vec![rate_limited_header_only("2")], ok()).await;
    let guard = default_guard();
    let exec = executor(&mock, &guard);

    let (data, _) = tokio::time::timeout(Duration::from_secs(300), exec.get_json_observed(PATH))
        .await
        .expect("read terminates inside the virtual budget")
        .expect("read succeeds")
        .expect("the waited-out retry returns Some(ok)");
    assert_eq!(data, serde_json::json!({"ok": true}));

    let gap = first_gap(&mock);
    assert!(
        gap >= Duration::from_secs(2),
        "control skipped the 2 s Retry-After wait: {gap:?}"
    );
    assert!(
        gap < Duration::from_secs(10),
        "control wait must stay the short header wait, got {gap:?}"
    );
    assert_eq!(guard.snapshot().invalid_requests_total, 1);
    mock.shutdown().await;
}

// (4) Clamp pin (passes before and after): the kick lane keeps the 60 s
// clamp. A 75 s header produces a wait at the clamp — at least ~59 s so a
// dropped wait fails, under 70 s so an unclamp-everything fix fails — then
// the kick lands on its second attempt.
#[tokio::test(start_paused = true)]
async fn kick_keeps_the_sixty_second_clamp() {
    let mock = FakeRest::start(
        vec![rate_limited_header_only("75")],
        FakeResponse::status(204),
    )
    .await;
    let guard = default_guard();
    let exec = executor(&mock, &guard);

    let result = tokio::time::timeout(
        Duration::from_secs(300),
        exec.kick_paced("2222", "3333", "retry-after clamp pin"),
    )
    .await
    .expect("kick terminates inside the virtual budget");
    assert_eq!(result.outcome, KickOutcome::Kicked);
    assert_eq!(result.attempts, 2, "one 429 plus one clamped-wait retry");

    let gap = first_gap(&mock);
    assert!(
        gap >= Duration::from_secs(59),
        "kick skipped the clamped Retry-After wait: {gap:?}"
    );
    assert!(
        gap < Duration::from_secs(70),
        "kick wait must stay clamped at 60 s, not honor the 75 s header: {gap:?}"
    );
    mock.shutdown().await;
}

// (5) Clamp pin (passes before and after): the publish lane keeps the 60 s
// clamp on the same terms as the kick lane.
#[tokio::test(start_paused = true)]
async fn publish_keeps_the_sixty_second_clamp() {
    let mock = FakeRest::start(
        vec![rate_limited_header_only("75")],
        FakeResponse::json(200, serde_json::json!([])),
    )
    .await;
    let guard = default_guard();
    let exec = executor(&mock, &guard);

    tokio::time::timeout(
        Duration::from_secs(300),
        exec.publish_guild_commands(1111, 2222, &[]),
    )
    .await
    .expect("publish terminates inside the virtual budget")
    .expect("publish succeeds after the clamped wait");

    let gap = first_gap(&mock);
    assert!(
        gap >= Duration::from_secs(59),
        "publish skipped the clamped Retry-After wait: {gap:?}"
    );
    assert!(
        gap < Duration::from_secs(70),
        "publish wait must stay clamped at 60 s, not honor the 75 s header: {gap:?}"
    );
    mock.shutdown().await;
}

// (6) Exhaustion pin (passes before and after): a route that genuinely
// keeps failing still burns exactly five tries and then stops — the
// unclamped wait must not become an unbounded retry — and the
// invalid-request budget still opens the breaker so the NEXT read is
// refused before it reaches the wire.
#[tokio::test(start_paused = true)]
async fn persistent_429_exhausts_five_tries_and_opens_invalid_budget() {
    let mock = FakeRest::start(vec![], rate_limited_header_only("0")).await;
    let guard = Arc::new(
        RateLimitGuard::new(GuardConfig {
            invalid_request_threshold: 3,
            ..GuardConfig::default()
        })
        .expect("fixture guard"),
    );
    let exec = executor(&mock, &guard);

    let exhausted = tokio::time::timeout(Duration::from_secs(300), exec.get_json_observed(PATH))
        .await
        .expect("read terminates inside the virtual budget")
        .expect("exhaustion is a clean None, not an error");
    assert!(
        exhausted.is_none(),
        "five spent tries end the read with None"
    );
    assert_eq!(
        mock.requests().len(),
        5,
        "exactly five wire tries for a genuinely failing route"
    );
    assert_eq!(guard.snapshot().invalid_requests_total, 5);
    assert!(
        guard.snapshot().breaker_open,
        "the invalid-request budget opened the breaker"
    );

    let refused = exec
        .get_json_observed(PATH)
        .await
        .expect_err("open breaker refuses the next read");
    assert!(
        refused.contains("circuit breaker"),
        "unexpected refusal: {refused}"
    );
    assert_eq!(
        mock.requests().len(),
        5,
        "the refused read never reached the wire"
    );
    mock.shutdown().await;
}
