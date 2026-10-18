//! Scope pins for the backfill `Retry-After` policy (review follow-up).
//!
//! The unclamped wait belongs to the backfill evidence reads
//! (`get_json_observed`, `get_json_checked`) and is bounded by a 5 min
//! ceiling per wait. The general `get_json` read — the containment
//! snapshot, website jobs and onboarding probe path — keeps the 60 s clamp,
//! so a long per-route wait cannot park a serialized worker.
//!
//! Loopback fixtures only, paused clock: no live Discord, no database, and
//! no minutes of wall time. Mock-side arrival times ride the same tokio
//! clock the executor sleeps on, so the gap between the two wire requests
//! is the wait the executor actually took.

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
// Minimal scripted HTTP server: replays a response queue (status + headers
// + body) while recording every request's method, path and mock-side
// arrival time.

#[derive(Debug, Clone)]
struct FakeResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl FakeResponse {
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

const PATH: &str = "/guilds/2222/roles";

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
/// outranks the header — the executor's own wait is the whole story.
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

// (1) The general read keeps the 60 s clamp: a 75 s header produces a wait
// at the clamp — at least ~59 s so a dropped wait fails, under 70 s so an
// unclamped wait fails — then the read lands on its second attempt. This is
// the containment snapshot / website-job path: it must never park for the
// full header.
#[tokio::test(start_paused = true)]
async fn general_read_keeps_the_sixty_second_clamp() {
    let mock = FakeRest::start(vec![rate_limited_header_only("75")], ok()).await;
    let guard = default_guard();
    let exec = executor(&mock, &guard);

    let data = tokio::time::timeout(Duration::from_secs(300), exec.get_json(PATH))
        .await
        .expect("read terminates inside the virtual budget")
        .expect("read succeeds")
        .expect("the clamped retry returns Some(ok)");
    assert_eq!(data, serde_json::json!({"ok": true}));

    let seen = mock.requests();
    assert_eq!(
        seen.len(),
        2,
        "one 429 plus one clamped-wait retry, never a storm"
    );
    assert!(seen
        .iter()
        .all(|r| r.method == "GET" && r.path.ends_with(PATH)));
    let gap = first_gap(&mock);
    assert!(
        gap >= Duration::from_secs(59),
        "general read skipped the clamped Retry-After wait: {gap:?}"
    );
    assert!(
        gap < Duration::from_secs(70),
        "general read must stay clamped at 60 s, not honor the 75 s header: {gap:?}"
    );
    mock.shutdown().await;
}

// (2) The backfill evidence read stops a huge header at the 5 min ceiling:
// an 86400 s header produces one ~300 s wait (not a day-long park), then
// the read lands on its second attempt with exactly one tally.
#[tokio::test(start_paused = true)]
async fn backfill_read_caps_huge_retry_after_at_ceiling() {
    let mock = FakeRest::start(vec![rate_limited_header_only("86400")], ok()).await;
    let guard = default_guard();
    let exec = executor(&mock, &guard);

    let (data, _) = tokio::time::timeout(Duration::from_secs(600), exec.get_json_observed(PATH))
        .await
        .expect("read terminates inside the virtual budget")
        .expect("read succeeds")
        .expect("the ceiling-capped retry returns Some(ok)");
    assert_eq!(data, serde_json::json!({"ok": true}));

    let gap = first_gap(&mock);
    assert!(
        gap >= Duration::from_secs(299),
        "backfill read cut the ceiling wait short: {gap:?}"
    );
    assert!(
        gap < Duration::from_secs(320),
        "backfill read must stop at the 5 min ceiling, not honor the 86400 s header: {gap:?}"
    );
    assert_eq!(
        guard.snapshot().invalid_requests_total,
        1,
        "exactly one invalid-request tally for the single 429"
    );
    mock.shutdown().await;
}
