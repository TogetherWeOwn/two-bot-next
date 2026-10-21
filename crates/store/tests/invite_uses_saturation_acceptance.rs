//! Acceptance tests: out-of-range invite `uses` saturate at `i32::MAX`
//! and the snapshot write proceeds.
//!
//! Protected test file: the build card implements against these tests and
//! must not edit them. If a pinned name below cannot be implemented as
//! written, stop and escalate instead of changing the tests.
//!
//! Contract pinned here (build card implements exactly this):
//!
//! - `PgInviteSnapshots::store_all` never panics on any `u64` `uses`.
//! - Values above `i32::MAX` persist saturated at `i32::MAX` (`load`
//!   returns `i32::MAX as u64`); the rest of the row is unchanged.
//! - Each saturated write emits a bounded `WARN` naming the guild and the
//!   invite code (decimal guild id and code identifiers only); the raw
//!   out-of-range counter never appears in logs.
//! - Values at or below `i32::MAX` round-trip byte-identical with no
//!   saturation warning naming their code.
//! - No other `store_all` behavior changes; the documented
//!   database-failure panics are untouched.
//!
//! Database tests, disposable database only (the same admission rules as
//! the `privacy_lock_tests` suite): `TWO_TEST_DATABASE_URL` on agent-testdb
//! or the CI service container with the `agent_test` identity, one
//! throwaway schema per test. Without the variable the tests skip; under
//! `GITHUB_ACTIONS` a missing variable fails closed. These tests run in
//! the ordinary workspace sweep with no per-test opt-out.

use std::sync::{Arc, OnceLock};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use two_bot_core::{InviteSnapshotStore, InviteState};
use two_bot_store::{migrate, PgInviteSnapshots};

const GUILD: u64 = 100_000_000_000_000_001;
const SATURATED: u64 = i32::MAX as u64;

struct Fixture {
    admin: Pool<Postgres>,
    pool: Pool<Postgres>,
    schema: String,
}

impl Fixture {
    async fn new(prefix: &str) -> Option<Self> {
        let url = match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(_) => {
                assert_ne!(
                    std::env::var("GITHUB_ACTIONS").as_deref(),
                    Ok("true"),
                    "CI must configure the invite saturation database suite"
                );
                eprintln!("SKIP invite saturation test: TWO_TEST_DATABASE_URL not set");
                return None;
            }
        };
        let options: PgConnectOptions = url.parse().expect("test database URL");
        let ci_loopback = options.get_host() == "localhost"
            && std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
        assert!(
            options.get_host() == "agent-testdb" || ci_loopback,
            "test-container-only URL"
        );
        assert_eq!(options.get_username(), "agent_test", "test identity only");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let schema = format!("{prefix}_{nonce}_{}", std::process::id());
        assert!(
            schema.len() <= 63
                && schema
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "safe unique test schema"
        );
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .expect("test database admin");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .expect("create owned test schema");
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await
            .expect("test database pool");
        migrate(&pool).await.expect("migrate owned test schema");
        Some(Self {
            admin,
            pool,
            schema,
        })
    }

    fn snapshots(&self) -> PgInviteSnapshots {
        PgInviteSnapshots::new(self.pool.clone())
    }

    async fn finish(self) {
        self.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .expect("drop owned test schema");
        self.admin.close().await;
    }
}

// One process-wide capture: a scoped subscriber drops events under parallel
// tests, so the log-reading tests serialize on LOG_GUARD instead. This
// crate has no `tracing-subscriber` dev-dependency, so the capture is a
// hand-rolled `tracing::Subscriber` over the main `tracing` dependency.
static LOG_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Default)]
struct Capture(Arc<std::sync::Mutex<Vec<String>>>);

struct FieldRecorder {
    parts: Vec<String>,
}

impl tracing::field::Visit for FieldRecorder {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.parts.push(format!("{}={value:?}", field.name()));
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut recorder = FieldRecorder { parts: Vec::new() };
        event.record(&mut recorder);
        let line = format!("{} {}", event.metadata().level(), recorder.parts.join(" "));
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(line);
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

impl Capture {
    fn shared() -> Self {
        static CAPTURE: OnceLock<Capture> = OnceLock::new();
        let capture = CAPTURE
            .get_or_init(|| {
                let capture = Capture::default();
                tracing::subscriber::set_global_default(capture.clone())
                    .expect("install log capture");
                capture
            })
            .clone();
        capture
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        capture
    }

    fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

fn invite(code: &str, uses: u64) -> InviteState {
    InviteState {
        code: code.to_owned(),
        uses,
        inviter_id: Some(123_456_789_012_345_678),
        channel_id: Some(321_098_765_432_109_876),
        max_uses: 0,
        temporary: false,
    }
}

fn expected(code: &str, uses: u64) -> InviteState {
    invite(code, uses)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uses_one_above_i32_max_saturates_and_write_proceeds() {
    let Some(fixture) = Fixture::new("invite_saturation").await else {
        return;
    };
    let snapshots = fixture.snapshots();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        snapshots.store_all(GUILD, &[invite("saturate-plus-one", SATURATED + 1)]);
    }));
    let loaded = snapshots.load(GUILD);
    fixture.finish().await;
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
    assert_eq!(
        loaded,
        vec![expected("saturate-plus-one", SATURATED)],
        "uses above i32::MAX must persist saturated"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uses_u64_max_saturates_at_i32_max() {
    let Some(fixture) = Fixture::new("invite_saturation").await else {
        return;
    };
    let snapshots = fixture.snapshots();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        snapshots.store_all(GUILD, &[invite("saturate-u64-max", u64::MAX)]);
    }));
    let loaded = snapshots.load(GUILD);
    fixture.finish().await;
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
    assert_eq!(
        loaded,
        vec![expected("saturate-u64-max", SATURATED)],
        "u64::MAX uses must persist saturated at i32::MAX"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saturated_write_warns_with_identifiers_and_no_raw_counter() {
    let _guard = LOG_GUARD.lock().await;
    let Some(fixture) = Fixture::new("invite_saturation").await else {
        return;
    };
    let capture = Capture::shared();
    let snapshots = fixture.snapshots();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        snapshots.store_all(GUILD, &[invite("saturate-warn", u64::MAX)]);
    }));
    fixture.finish().await;
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
    let lines = capture.lines();
    let guild = GUILD.to_string();
    let raw = u64::MAX.to_string();
    assert!(
        lines.iter().any(|line| line.starts_with("WARN")),
        "a saturated write must warn, got: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("WARN") && line.contains("saturate-warn")),
        "the saturation warn must name the invite code, got: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.starts_with("WARN")
            && line.contains("saturate-warn")
            && line.contains(guild.as_str())),
        "the saturation warn must name the guild, got: {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains(raw.as_str())),
        "the saturation warn must not dump the raw counter, got: {lines:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_uses_round_trip_byte_identical() {
    let Some(fixture) = Fixture::new("invite_saturation").await else {
        return;
    };
    let snapshots = fixture.snapshots();
    let states = [
        invite("normal-zero", 0),
        invite("normal-small", 7),
        invite("normal-max", SATURATED),
    ];
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        snapshots.store_all(GUILD, &states);
        snapshots.load(GUILD)
    }));
    fixture.finish().await;
    let mut loaded = match outcome {
        Ok(loaded) => loaded,
        Err(payload) => std::panic::resume_unwind(payload),
    };
    loaded.sort_by(|left, right| left.code.cmp(&right.code));
    assert_eq!(
        loaded,
        vec![
            expected("normal-max", SATURATED),
            expected("normal-small", 7),
            expected("normal-zero", 0),
        ],
        "in-range uses must round-trip exactly"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_i32_max_writes_without_saturation_warn() {
    let _guard = LOG_GUARD.lock().await;
    let Some(fixture) = Fixture::new("invite_saturation").await else {
        return;
    };
    let capture = Capture::shared();
    let snapshots = fixture.snapshots();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        snapshots.store_all(GUILD, &[invite("exact-max-quiet", SATURATED)]);
    }));
    fixture.finish().await;
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
    let lines = capture.lines();
    assert!(
        !lines
            .iter()
            .any(|line| line.contains("WARN") && line.contains("exact-max-quiet")),
        "an exact i32::MAX write is not a saturation, got: {lines:?}"
    );
}
