//! Failing-first acceptance for surfacing the duplicate pool-registration count.
//!
//! Protected test file. The Builder builds on this card's branch and never
//! edits this file. If a pinned name below cannot be implemented as written,
//! stop and return the correction to the author instead of changing the tests.
//!
//! Contract under test (builder: implement exactly this):
//! - A rejected `register_pool` increments the existing in-memory
//!   `duplicate_registration_count()` (already true; pinned by the existing
//!   `double_registration_keeps_first_pool` unit test and re-pinned here via
//!   an isolated child process so the new file also fails if the counter
//!   regresses).
//! - The count surfaces with a stable name on the `/ops` roll-up OR the
//!   `/metrics` scrape (builder's choice, either or both):
//!   - `/ops`: JSON field named exactly `duplicate_pool_registrations`
//!     (top-level in `/ops/summary`, or inside the `pool` object when a pool
//!     is registered). Value is a JSON number equal to the count.
//!   - Scrape: metric named exactly
//!     `two_bot_db_pool_duplicate_registrations`, label-free, `# TYPE`
//!     `gauge`, sample value equal to the count. The name joins the existing
//!     `two_bot_db_pool_*` gauge family (`configured`, `connections`, `idle`,
//!     `max`); the roadmap shorthand `duplicate_pool_registrations` is the
//!     `/ops` field and the scrape suffix.
//! - Zero-registration state renders zero, not absent-vs-zero ambiguity: with
//!   no pool registered (unconfigured boot, `pool: null`) and count zero, the
//!   gauge is still present with value `0` on the chosen surface. A
//!   pool-object-only field cannot satisfy this (the pool object is `null`
//!   when unconfigured), so zero-state requires the top-level `/ops` field or
//!   the scrape metric. Absent, `null`, or a missing series fails.
//!
//! Isolation: the counter and the global pool slot are process-global. Each
//! behavioral assertion runs in a fresh child process (the test binary
//! re-executed with `<probe> --exact --nocapture`, following the repo's
//! `backup_cli::tests::tls_admission_guard_child` /
//! `admission_test_support::run_probe` recipe) so counts are deterministic
//! (`0` with no registrations, `1` with one reject) and no parent-process
//! global is mutated. Parent tests only spawn children and parse receipt
//! lines; they never call `register_pool` themselves. The probes are plain
//! tests gated on `DUPLICATE_POOL_CHILD_PROBE`: in normal suite runs the env
//! var is unset and they return immediately without touching any global (no
//! `#[ignore]`: the floor guard bans new skips, and an ignored probe cannot
//! be re-selected reliably). This is offline-only: lazy pools
//! (`connect_lazy`, no I/O), in-process `oneshot` router calls, no network,
//! no database, no `TWO_TEST_DATABASE_URL`, no Discord.
//!
//! RED now (behavioral, not compile): the increment control passes, the two
//! surfacing tests fail with assertion panics naming the absent gauge. GREEN
//! after the builder wires the existing counter to either surface with the
//! exact names above. No new symbol is named here, so the baseline compiles.
#![cfg(test)]

use std::io::Write as _;
use std::process::Command;
use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use tokio::sync::RwLock;
use tower::ServiceExt as _;

use crate::gateway::GatewayState;
use crate::server::SharedState;

/// Stable `/ops` field name (top-level or inside `pool`).
const OPS_FIELD: &str = "duplicate_pool_registrations";
/// Stable scrape metric name (label-free gauge in the pool family).
const SCRAPE_METRIC: &str = "two_bot_db_pool_duplicate_registrations";
/// Gates the child probes: only a parent-spawned child sets this. Normal
/// suite runs leave it unset, so the probes return without touching globals.
const PROBE_ENV: &str = "DUPLICATE_POOL_CHILD_PROBE";

fn lazy_pool(max: u32) -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(max)
        .connect_lazy("postgres://agent_test@agent-testdb/agent_test")
        .expect("lazy pool builds without I/O")
}

fn test_app() -> Router {
    let state = SharedState::new(Arc::new(RwLock::new(GatewayState::Unconfigured)), None);
    crate::server::router_with_guard(
        state,
        crate::jobs::statuses(&[], true),
        two_bot_discord::ratelimit_guard::process_guard(),
    )
}

async fn fetch_ops_summary_json(app: &Router) -> serde_json::Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ops/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1_048_576)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn fetch_scrape_text(app: &Router) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 65_536)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn print_receipt(key: &str, value: &str) {
    // Leading newline separates the receipt from libtest's prefix.
    writeln!(std::io::stdout().lock(), "\n{key}={value}").expect("write child receipt");
}

fn scrape_duplicate_line(scrape: &str) -> &str {
    scrape
        .lines()
        .find(|line| {
            line.starts_with(SCRAPE_METRIC)
                && line.as_bytes().get(SCRAPE_METRIC.len()) == Some(&b' ')
        })
        .unwrap_or("ABSENT")
}

fn scrape_duplicate_type(scrape: &str) -> String {
    let prefix = format!("# TYPE {SCRAPE_METRIC} ");
    scrape
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "absent".to_owned())
}

/// Fresh-process probe: one install plus one reject, then the count and both
/// surfaces. Runs only as a parent-spawned child (`PROBE_ENV` set); in normal
/// suite runs it returns immediately so it never mutates the shared process.
#[tokio::test]
async fn duplicate_pool_child_probe_with_reject() {
    if std::env::var_os(PROBE_ENV).is_none() {
        return;
    }
    let first = lazy_pool(3);
    let second = lazy_pool(7);
    assert!(
        crate::metrics_http::register_pool(first),
        "first registration in a fresh child must install"
    );
    assert!(
        !crate::metrics_http::register_pool(second),
        "second registration in a fresh child must reject"
    );
    let count = crate::metrics_http::duplicate_registration_count();
    assert_eq!(count, 1, "fresh child with one reject must count 1");
    print_receipt("DUPLICATE_COUNT", &count.to_string());

    let app = test_app();
    let summary = fetch_ops_summary_json(&app).await;
    print_receipt(
        "OPS_SUMMARY",
        &serde_json::to_string(&summary).expect("summary serializes"),
    );

    let scrape = fetch_scrape_text(&app).await;
    print_receipt("SCRAPE_DUPLICATE_LINE", scrape_duplicate_line(&scrape));
    print_receipt("SCRAPE_DUPLICATE_TYPE", &scrape_duplicate_type(&scrape));
}

/// Fresh-process probe: no registrations at all (count zero, pool `None`).
/// Runs only as a parent-spawned child (`PROBE_ENV` set); in normal suite
/// runs it returns immediately so it never mutates the shared process.
#[tokio::test]
async fn duplicate_pool_child_probe_zero() {
    if std::env::var_os(PROBE_ENV).is_none() {
        return;
    }
    let count = crate::metrics_http::duplicate_registration_count();
    assert_eq!(count, 0, "fresh child with no registrations must count 0");
    print_receipt("DUPLICATE_COUNT", &count.to_string());

    let app = test_app();
    let summary = fetch_ops_summary_json(&app).await;
    print_receipt(
        "OPS_SUMMARY",
        &serde_json::to_string(&summary).expect("summary serializes"),
    );

    let scrape = fetch_scrape_text(&app).await;
    print_receipt("SCRAPE_DUPLICATE_LINE", scrape_duplicate_line(&scrape));
    print_receipt("SCRAPE_DUPLICATE_TYPE", &scrape_duplicate_type(&scrape));
}

fn run_child_probe(child_fn: &str) -> (String, String, bool) {
    // Literal path, like the `backup_cli` / `admission_test_support` recipe:
    // libtest names exclude the crate name, while `module_path!()` includes
    // it (`two_bot::...`), which `--exact` then never matches (infra RED:
    // `running 0 tests`, everything filtered out).
    let test_path = format!("duplicate_pool_registration_acceptance_tests::{child_fn}");
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([&test_path, "--exact", "--nocapture"])
        .env(PROBE_ENV, "1")
        .output()
        .expect("spawn isolated child probe (infra: failed to spawn test binary)");
    let stdout = String::from_utf8(output.stdout).expect("child stdout is UTF-8");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stdout.contains("running 1 test"),
        "child probe filter `{test_path}` must select exactly the probe (infra: filter missed); stderr:\n{stderr}\nstdout:\n{stdout}"
    );
    (stdout, stderr, output.status.success())
}

fn receipt(stdout: &str, key: &str) -> String {
    let prefix = format!("{key}=");
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()).map(str::to_owned))
        .unwrap_or_else(|| {
            panic!("missing {key} receipt (child crashed or infra); stdout:\n{stdout}")
        })
}

fn scrape_value_is(line: &str, kind: &str, expected: u64) -> bool {
    if line == "ABSENT" {
        return false;
    }
    let mut parts = line.split_whitespace();
    let name_ok = parts.next() == Some(SCRAPE_METRIC);
    let value_ok = parts.next().and_then(|v| v.parse::<u64>().ok()) == Some(expected);
    let no_extra = parts.next().is_none();
    name_ok && value_ok && no_extra && kind == "gauge"
}

/// Control (GREEN now): a rejected `register_pool` increments the shared
/// count. Proved in a fresh child process so the parent never mutates the
/// global pool slot.
#[test]
fn rejected_registration_increments_the_shared_count() {
    let (stdout, stderr, success) = run_child_probe("duplicate_pool_child_probe_with_reject");
    assert!(
        success,
        "child increment probe must pass (increment broken or infra); stderr:\n{stderr}\nstdout:\n{stdout}"
    );
    let count: u64 = receipt(&stdout, "DUPLICATE_COUNT")
        .parse()
        .expect("count receipt parses as u64");
    assert_eq!(
        count, 1,
        "one rejected register_pool must count 1 in a fresh process"
    );
}

/// RED now: after one rejected registration (count 1 in the child), the count
/// surfaces with a stable name on `/ops` or the scrape. Accepts the top-level
/// `/ops` field, the `pool`-object field (pool is registered in this child),
/// or the scrape gauge; at least one must equal the count.
#[test]
fn rejected_count_surfaces_on_ops_or_scrape_with_stable_name() {
    let (stdout, stderr, success) = run_child_probe("duplicate_pool_child_probe_with_reject");
    assert!(
        success,
        "child probe must pass (increment broken or infra); stderr:\n{stderr}\nstdout:\n{stdout}"
    );
    let count: u64 = receipt(&stdout, "DUPLICATE_COUNT")
        .parse()
        .expect("count receipt parses as u64");
    let summary: serde_json::Value =
        serde_json::from_str(&receipt(&stdout, "OPS_SUMMARY")).expect("ops receipt is JSON");
    let scrape_line = receipt(&stdout, "SCRAPE_DUPLICATE_LINE");
    let scrape_type = receipt(&stdout, "SCRAPE_DUPLICATE_TYPE");

    let ops_top_ok = summary.get(OPS_FIELD).and_then(|v| v.as_u64()) == Some(count);
    let ops_pool_ok = summary
        .get("pool")
        .and_then(|pool| pool.get(OPS_FIELD))
        .and_then(|v| v.as_u64())
        == Some(count);
    let scrape_ok = scrape_value_is(&scrape_line, &scrape_type, count);

    assert!(
        ops_top_ok || ops_pool_ok || scrape_ok,
        "duplicate count {count} must surface with a stable name on /ops top-level `{OPS_FIELD}`, /ops pool `{OPS_FIELD}`, or scrape `{SCRAPE_METRIC}` gauge; got ops_top={:?}, ops_pool={:?}, scrape_line={scrape_line:?}, scrape_type={scrape_type:?}",
        summary.get(OPS_FIELD),
        summary.get("pool").and_then(|pool| pool.get(OPS_FIELD)),
    );
}

/// RED now: with zero registrations (fresh child, pool `None`, count 0), the
/// gauge renders `0`, not absent or `null`. Pool-object-only cannot satisfy
/// this (the pool object is `null` when unconfigured), so zero-state requires
/// the top-level `/ops` field or the scrape gauge.
#[test]
fn zero_state_renders_zero_not_absent() {
    let (stdout, stderr, success) = run_child_probe("duplicate_pool_child_probe_zero");
    assert!(
        success,
        "zero child probe must pass (infra); stderr:\n{stderr}\nstdout:\n{stdout}"
    );
    let count: u64 = receipt(&stdout, "DUPLICATE_COUNT")
        .parse()
        .expect("count receipt parses as u64");
    assert_eq!(count, 0, "zero child must count 0");
    let summary: serde_json::Value =
        serde_json::from_str(&receipt(&stdout, "OPS_SUMMARY")).expect("ops receipt is JSON");
    let scrape_line = receipt(&stdout, "SCRAPE_DUPLICATE_LINE");
    let scrape_type = receipt(&stdout, "SCRAPE_DUPLICATE_TYPE");

    let ops_top_zero = summary.get(OPS_FIELD).and_then(|v| v.as_u64()) == Some(0);
    let scrape_zero = scrape_value_is(&scrape_line, &scrape_type, 0);

    assert!(
        ops_top_zero || scrape_zero,
        "zero registrations must render 0 (not absent/null) on /ops top-level `{OPS_FIELD}` or scrape `{SCRAPE_METRIC}` gauge; got ops_top={:?}, pool={:?}, scrape_line={scrape_line:?}, scrape_type={scrape_type:?} (pool-object alone is insufficient: pool is null when unconfigured)",
        summary.get(OPS_FIELD),
        summary.get("pool"),
    );
}
