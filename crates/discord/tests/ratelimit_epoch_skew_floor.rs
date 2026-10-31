//! Acceptance: floor exhausted-bucket waits under clock skew.
//!
//! Contract (from the card spec, blind to the implementation):
//!
//! * A wait derived from the absolute `X-RateLimit-Reset` epoch is clamped
//!   to `[0, window]`, where `window` is the guard's configured
//!   [`GuardConfig::window`]. The existing 250 ms safety pad is added on
//!   top of the clamped/derived wait, exactly as today.
//! * A past epoch with `X-RateLimit-Remaining: 0` (Discord and the local
//!   wall clock disagree) still installs a bucket deadline: a floor of
//!   1 s, plus the 250 ms pad, so admission opens again after ~1.25 s.
//!   The skew is logged (a log line naming the skew).
//! * A `Reset` 5 s in the past with `Remaining: 0` therefore gates
//!   admission and admits after ~1.25 s — never immediately, and never
//!   for a duration proportional to the skew.
//! * Future epochs keep the current deadline behaviour (wait until the
//!   epoch, plus the 250 ms pad).
//! * The floor applies only to an exhausted bucket (`Remaining: 0`) on
//!   the observed route/bucket. Responses with remaining budget or no
//!   `Remaining` header install nothing, and the floor never becomes a
//!   guard-wide pause.
//!
//! All tests use synthetic epochs (real wall clock offset) against a
//! paused Tokio clock, so admission elapsed time is the exact installed
//! wait. No live Discord, database or network is involved.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use http::{HeaderMap, HeaderName, HeaderValue};
use tokio::time::Instant;
use two_bot_discord::ratelimit_guard::{GuardConfig, RateLimitGuard};

const ROUTE: &str = "GET /guilds/3/roles";

fn guard() -> RateLimitGuard {
    RateLimitGuard::new(GuardConfig::default()).expect("default guard config is valid")
}

/// Absolute `X-RateLimit-Reset` header value `offset` from the current
/// wall clock (past when `future` is false), in epoch seconds.
fn reset_epoch(offset: Duration, future: bool) -> String {
    let target = if future {
        SystemTime::now() + offset
    } else {
        SystemTime::now() - offset
    };
    target
        .duration_since(UNIX_EPOCH)
        .expect("synthetic epoch is after the Unix epoch")
        .as_secs_f64()
        .to_string()
}

/// Bucket headers with an absolute reset epoch and no relative
/// `Reset-After`, so the wait must be derived from the epoch alone.
fn epoch_headers(remaining: Option<&str>, reset: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-ratelimit-bucket"),
        HeaderValue::from_static("bucket-epoch-skew"),
    );
    if let Some(remaining) = remaining {
        headers.insert(
            HeaderName::from_static("x-ratelimit-remaining"),
            HeaderValue::from_str(remaining).expect("remaining header value"),
        );
    }
    headers.insert(
        HeaderName::from_static("x-ratelimit-reset"),
        HeaderValue::from_str(reset).expect("reset header value"),
    );
    headers
}

/// Paused-clock time until `admit_route` admits `route`.
async fn admitted_after(guard: &RateLimitGuard, route: &str) -> Duration {
    let start = Instant::now();
    guard
        .admit_route(route, false)
        .await
        .expect("route admission must succeed once its bucket deadline passes");
    start.elapsed()
}

fn assert_wait_in(wait: Duration, low: Duration, high: Duration, what: &str) {
    assert!(
        wait >= low && wait <= high,
        "{what}: admission wait {wait:?} must be within [{low:?}, {high:?}]"
    );
}

// ── The floor: past epoch, exhausted bucket ───────────────────────────────

#[tokio::test(start_paused = true)]
async fn reset_five_seconds_in_past_with_remaining_zero_gates_then_admits_after_floor() {
    let guard = guard();
    let reset = reset_epoch(Duration::from_secs(5), false);
    guard.observe_bucket(ROUTE, &epoch_headers(Some("0"), &reset));

    let start = Instant::now();
    let gated = tokio::time::timeout(Duration::from_secs(1), guard.admit_route(ROUTE, false)).await;
    assert!(
        gated.is_err(),
        "a past reset with Remaining: 0 must still gate admission for the floor, not admit immediately"
    );
    guard
        .admit_route(ROUTE, false)
        .await
        .expect("admission must open once the floor elapses");
    assert_wait_in(
        start.elapsed(),
        Duration::from_millis(1_150),
        Duration::from_millis(1_400),
        "Reset 5s in the past with Remaining: 0 admits after the 1s + 250ms floor",
    );
}

#[tokio::test(start_paused = true)]
async fn far_past_reset_with_remaining_zero_waits_only_the_floor_not_the_skew() {
    let guard = guard();
    let reset = reset_epoch(Duration::from_secs(3_600), false);
    guard.observe_bucket(ROUTE, &epoch_headers(Some("0"), &reset));

    assert_wait_in(
        admitted_after(&guard, ROUTE).await,
        Duration::from_millis(1_150),
        Duration::from_millis(1_400),
        "far-past Reset with Remaining: 0 waits the 1s + 250ms floor, not the skew size",
    );
}

// ── The clamp: epoch-derived waits stay within [0, window] ───────────────

#[tokio::test(start_paused = true)]
async fn far_future_reset_is_clamped_to_the_configured_window() {
    let guard = RateLimitGuard::new(GuardConfig {
        window: Duration::from_secs(2),
        ..GuardConfig::default()
    })
    .expect("small-window guard config is valid");
    let reset = reset_epoch(Duration::from_secs(300), true);
    guard.observe_bucket(ROUTE, &epoch_headers(Some("0"), &reset));

    assert_wait_in(
        admitted_after(&guard, ROUTE).await,
        Duration::from_millis(1_900),
        Duration::from_millis(2_600),
        "epoch-derived wait clamps to the 2s window (plus the 250ms pad), not the raw 300s epoch gap",
    );
}

// ── Future epochs keep current deadline behaviour ─────────────────────────

#[tokio::test(start_paused = true)]
async fn future_reset_keeps_the_epoch_deadline_plus_pad() {
    let guard = guard();
    let reset = reset_epoch(Duration::from_secs(5), true);
    guard.observe_bucket(ROUTE, &epoch_headers(Some("0"), &reset));

    assert_wait_in(
        admitted_after(&guard, ROUTE).await,
        Duration::from_millis(4_500),
        Duration::from_millis(5_350),
        "future Reset keeps the current deadline (epoch + 250ms pad), not the skew floor",
    );
}

#[tokio::test(start_paused = true)]
async fn near_future_reset_under_the_window_is_not_floored_or_shrunk() {
    let guard = guard();
    let reset = reset_epoch(Duration::from_millis(700), true);
    guard.observe_bucket(ROUTE, &epoch_headers(Some("0"), &reset));

    assert_wait_in(
        admitted_after(&guard, ROUTE).await,
        Duration::from_millis(650),
        Duration::from_millis(1_100),
        "near-future Reset waits the epoch gap + 250ms pad (about 950ms), under the 1.25s floor",
    );
}

// ── The floor is only for an exhausted bucket on the observed route ──────

#[tokio::test(start_paused = true)]
async fn past_reset_with_remaining_budget_does_not_gate() {
    let guard = guard();
    let reset = reset_epoch(Duration::from_secs(5), false);
    guard.observe_bucket(ROUTE, &epoch_headers(Some("3"), &reset));

    assert_wait_in(
        admitted_after(&guard, ROUTE).await,
        Duration::ZERO,
        Duration::from_millis(200),
        "past Reset with remaining budget installs no floor",
    );
}

#[tokio::test(start_paused = true)]
async fn past_reset_without_remaining_header_does_not_gate() {
    let guard = guard();
    let reset = reset_epoch(Duration::from_secs(5), false);
    guard.observe_bucket(ROUTE, &epoch_headers(None, &reset));

    assert_wait_in(
        admitted_after(&guard, ROUTE).await,
        Duration::ZERO,
        Duration::from_millis(200),
        "past Reset without a Remaining header installs no floor",
    );
}

#[tokio::test(start_paused = true)]
async fn floor_does_not_gate_a_different_top_level_resource() {
    let guard = guard();
    let reset = reset_epoch(Duration::from_secs(5), false);
    guard.observe_bucket(ROUTE, &epoch_headers(Some("0"), &reset));

    assert_wait_in(
        admitted_after(&guard, "GET /guilds/4/roles").await,
        Duration::ZERO,
        Duration::from_millis(200),
        "the skew floor is a per-bucket deadline, never a guard-wide pause",
    );
}

// ── The skew is logged ─────────────────────────────────────────────────────

thread_local! {
    static CAPTURE_BUF: std::cell::RefCell<Option<Vec<u8>>> =
        const { std::cell::RefCell::new(None) };
}

/// `io::Write` that appends to the calling thread's capture buffer, when
/// one is armed. Events on threads without a buffer are dropped.
struct ThreadLocalSink;

impl std::io::Write for ThreadLocalSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURE_BUF.with(|cell| {
            if let Some(vec) = cell.borrow_mut().as_mut() {
                vec.extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One process-global subscriber keeps callsite interest enabled under
/// parallel tests; the thread-local writer keeps captures isolated. The
/// Tokio test runtime is current-thread, so observation and admission
/// log on the test's own thread.
fn ensure_global_subscriber() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(|| ThreadLocalSink)
            .with_max_level(tracing::Level::INFO)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

#[tokio::test(start_paused = true)]
async fn past_reset_with_remaining_zero_logs_the_skew() {
    ensure_global_subscriber();
    let guard = guard();
    let reset = reset_epoch(Duration::from_secs(5), false);

    CAPTURE_BUF.with(|cell| *cell.borrow_mut() = Some(Vec::new()));
    guard.observe_bucket(ROUTE, &epoch_headers(Some("0"), &reset));
    let admitted = guard.admit_route(ROUTE, false).await;
    let bytes = CAPTURE_BUF.with(|cell| cell.borrow_mut().take().unwrap_or_default());

    assert!(
        admitted.is_ok(),
        "admission must open after the floor while capturing logs: {admitted:?}"
    );
    let logs: Vec<String> = String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(
        logs.iter()
            .any(|line| line.to_ascii_lowercase().contains("skew")),
        "installing the floor for a past reset must log the clock skew; captured: {logs:?}"
    );
}
