//! Acceptance tests: recurring-series keying + DST/UTC correctness.
//!
//! Epic `events: auto voice channel per event`, host decision D12 + accepted
//! changes R19 (timestamptz, DB-side window, local render on web only) and R20
//! (series keying). Failing-first tests for the recurring/DST build card.
//!
//! PROTECTED TEST FILE: the paired Builder builds on this branch and must
//! never edit this file. If a pinned signature below cannot be implemented
//! as written, stop and escalate instead of changing the tests.
//!
//! Contract pinned here (the Builder implements exactly these inherent
//! methods on [`two_bot_store::Store`]; nothing else in this file may be
//! reimplemented or shadowed):
//!
//! - `sync_event_voice_series(&self, event_id: &str,
//!   occurrences: &[(i64, Option<i64>, EventStatus)]) -> Result<Vec<i64>,
//!   sqlx::Error`
//!   Reconciles one mirror snapshot for a Discord scheduled event with the
//!   tracked `event_voice_channels` rows. Each occurrence tuple is
//!   `(starts_at_unix, ends_at_unix, status)` with unix seconds. Returns
//!   one row id per input occurrence, aligned with the input order:
//!   an occurrence whose start matches a tracked row resolves to that row;
//!   a `starts_at` shift of a tracked occurrence UPDATES that row (same
//!   id, channel kept); a genuinely new start INSERTS a new row (new id);
//!   a `Cancelled` occurrence closes its own row only (status flips,
//!   siblings untouched). The shift-vs-new matching rule between those
//!   poles is the Builder's choice; the scenarios below are the law.
//! - `due_event_voice_occurrences(&self, now_unix: i64)
//!   -> Result<Vec<i64>, sqlx::Error>`
//!   The R1 create window as one DB-side query over timestamptz columns:
//!   `status = SCHEDULED AND voice_channel_id IS NULL AND starts_at <=
//!   now + 15m AND starts_at > now - 5m`, row ids ordered by `starts_at`
//!   ascending. Takes only a UTC instant: no timezone or local-time input
//!   exists on this seam, so tick maths cannot depend on local time. The
//!   window predicate MUST be evaluated in SQL against timestamptz
//!   columns; fetching rows and filtering in Rust does not satisfy R19
//!   even where these tests would pass, and storing unix epochs in a
//!   bigint instead of timestamptz does not satisfy R19 either. The table
//!   card asserts the column types directly; these tests prove the
//!   behavior (a `timestamp without time zone` column fails the DST and
//!   session-timezone tests because PG interprets the values in the
//!   session zone).
//! - `read_event_voice_rows(&self, event_id: &str)
//!   -> Result<Vec<(i64, i64, Option<i64>, Option<String>, String)>,
//!   sqlx::Error>`
//!   Read-only inspection seam for assertions: one tuple per tracked row
//!   for the event, `(row_id, starts_at_unix, ends_at_unix,
//!   voice_channel_id, status_word)`, ordered by `starts_at` ascending.
//!   Status words are the [`EventStatus::as_str`] vocabulary
//!   (`"scheduled"`, `"cancelled"`).
//! - `bind_event_voice_channel(&self, row_id: i64, channel_id: &str)
//!   -> Result<(), sqlx::Error>`
//!   Fixture seam standing in for the ticker's Discord create: records the
//!   voice channel id on an existing row, nothing else.
//!
//! Baseline compilation: [`BaselineEventVoice`] provides stub
//! implementations of the four methods so this file compiles on a tree
//! without the feature. Every stub returns a "not implemented" error, so
//! the six behavioral tests fail until the Builder's inherent methods
//! (which always win over trait methods) land. The explicit-calls control
//! (one qualified call per trait method) keeps the trait live after GREEN:
//! rustc warns per-method, so inherent shadowing would otherwise trip
//! `clippy -D warnings`, which the Builder could not fix without editing
//! this file.
//!
//! DST fixtures (America/New_York weekly 20:00 series; epochs verified
//! with `date -u` and cross-checked by PG's own timestamptz parser in the
//! DB control test):
//!
//! - A: Sun 2025-10-26 20:00 EDT = 2025-10-27T00:00:00Z = 1761523200
//! - B: Sun 2025-11-02 20:00 EST = 2025-11-03T01:00:00Z = 1762131600
//!   (fall-back night: 02:00 EDT -> 01:00 EST; 20:00 is safely EST)
//! - C: Sun 2025-11-09 20:00 EST = 2025-11-10T01:00:00Z = 1762736400
//! - D: Sun 2025-03-02 20:00 EST = 2025-03-03T01:00:00Z = 1740963600
//! - D': Sun 2025-03-09 20:00 EDT = 2025-03-10T00:00:00Z = 1741564800
//!   (spring-forward day: 02:00 EST -> 03:00 EDT; 20:00 is safely EDT)
//! - E: Sun 2025-03-16 20:00 EDT = 2025-03-17T00:00:00Z = 1742169600
//! - B': Mon 2025-11-03 20:00 EST = 2025-11-04T01:00:00Z = 1762218000
//!
//! The traps: B - A is 608400s (the 169-hour fall-back week), D' - D is
//! 601200s (the 167-hour spring week), while C - B and E - D' are exactly
//! 604800s. Any fixed-offset or local-time T-15 computation fires the
//! post-transition occurrences an hour off and fails the window tests.
//!
//! Opt-in database tests, disposable databases/schemas only (the same
//! admission rules as `postgres.rs`): agent-testdb or a CI service
//! container, the `agent_test` identity, one throwaway schema per test
//! (one throwaway database per timezone in the session-timezone test).
//! Without `TWO_TEST_DATABASE_URL` the DB tests skip vacuously; under
//! `GITHUB_ACTIONS=true` a missing URL is a loud failure, never a skip.
//! No live provider call, no real credential, `DATABASE_URL` never
//! consulted, staging/production hosts refused by the URL guard.

use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres, Row};
use two_bot_core::scheduled_events::EventStatus;
use two_bot_store::{migrate, Store, DB_POOL_MAX};

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

// Pinned UTC instants (see module docs for the DST derivation).
const A_START: i64 = 1_761_523_200;
const A_T15: i64 = 1_761_522_300;
const B_START: i64 = 1_762_131_600;
const B_T15: i64 = 1_762_130_700;
const C_START: i64 = 1_762_736_400;
const C_T15: i64 = 1_762_735_500;
const D_START: i64 = 1_740_963_600;
const DP_START: i64 = 1_741_564_800;
const E_START: i64 = 1_742_169_600;
const BP_START: i64 = 1_762_218_000;

const TWO_HOURS: i64 = 7_200;
const FIFTEEN_MINUTES: i64 = 900;
const FIVE_MINUTES: i64 = 300;

const SERIES_FALL: &str = "200000000000000001";
const SERIES_SPRING: &str = "200000000000000002";
const SERIES_OTHER: &str = "200000000000000003";
const CHANNEL_A: &str = "100000000000000001";
const CHANNEL_B: &str = "100000000000000002";
const CHANNEL_C: &str = "100000000000000003";
const CHANNEL_D: &str = "100000000000000004";

const NOT_IMPLEMENTED: &str = "event voice recurring keying not implemented (acceptance baseline)";

/// Baseline adapter for the not-yet-built keying seam: every method errors
/// until the Builder's same-signature inherent methods on [`Store`] shadow
/// it. Seams use unix seconds and std/core types only so the contract
/// compiles with no new dependencies.
trait BaselineEventVoice {
    async fn sync_event_voice_series(
        &self,
        event_id: &str,
        occurrences: &[(i64, Option<i64>, EventStatus)],
    ) -> Result<Vec<i64>, sqlx::Error>;

    async fn due_event_voice_occurrences(&self, now_unix: i64) -> Result<Vec<i64>, sqlx::Error>;

    async fn read_event_voice_rows(
        &self,
        event_id: &str,
    ) -> Result<Vec<(i64, i64, Option<i64>, Option<String>, String)>, sqlx::Error>;

    async fn bind_event_voice_channel(
        &self,
        row_id: i64,
        channel_id: &str,
    ) -> Result<(), sqlx::Error>;
}

impl BaselineEventVoice for Store {
    async fn sync_event_voice_series(
        &self,
        _event_id: &str,
        _occurrences: &[(i64, Option<i64>, EventStatus)],
    ) -> Result<Vec<i64>, sqlx::Error> {
        Err(sqlx::Error::protocol(NOT_IMPLEMENTED))
    }

    async fn due_event_voice_occurrences(&self, _now_unix: i64) -> Result<Vec<i64>, sqlx::Error> {
        Err(sqlx::Error::protocol(NOT_IMPLEMENTED))
    }

    async fn read_event_voice_rows(
        &self,
        _event_id: &str,
    ) -> Result<Vec<(i64, i64, Option<i64>, Option<String>, String)>, sqlx::Error> {
        Err(sqlx::Error::protocol(NOT_IMPLEMENTED))
    }

    async fn bind_event_voice_channel(
        &self,
        _row_id: i64,
        _channel_id: &str,
    ) -> Result<(), sqlx::Error> {
        Err(sqlx::Error::protocol(NOT_IMPLEMENTED))
    }
}

/// One occurrence tuple in contract order: `(starts_at_unix, ends_at_unix,
/// status)`.
fn occ(start: i64, ends: Option<i64>, status: EventStatus) -> (i64, Option<i64>, EventStatus) {
    (start, ends, status)
}

fn scheduled(start: i64) -> (i64, Option<i64>, EventStatus) {
    occ(start, Some(start + TWO_HOURS), EventStatus::Scheduled)
}

/// The disposable-test-database URL, or `None` when the test must skip.
/// Only an absent URL skips: a configured URL must name the disposable
/// test container (or the ephemeral CI service); anything else fails
/// setup loudly instead of touching a real database.
fn database_url() -> Option<String> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the event voice recurring acceptance database"
            );
            eprintln!("SKIP event voice recurring acceptance: TWO_TEST_DATABASE_URL not set");
            None
        }
    }
}

fn guarded_options(url: &str) -> PgConnectOptions {
    let options = PgConnectOptions::from_str(url).expect("invalid TWO_TEST_DATABASE_URL");
    let host = options.get_host();
    assert!(
        host == "agent-testdb"
            || (host == "localhost" && std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")),
        "test-container-only URL, got host {host}"
    );
    assert_eq!(
        options.get_username(),
        "agent_test",
        "test-container-only user"
    );
    options
}

struct Fixture {
    admin: Pool<Postgres>,
    pool: Pool<Postgres>,
    schema: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let url = database_url()?;
        let options = guarded_options(&url);
        let admin = two_bot_store::connect_pool_with_tls(
            &url,
            two_bot_core::database_tls::TlsPolicy::LocalOnly,
        )
        .await
        .expect("test DB connect failed");
        let schema = format!(
            "evr_test_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(DB_POOL_MAX)
            .connect_with(options.options([
                ("search_path", schema.clone()),
                ("statement_timeout", "15000ms".to_owned()),
            ]))
            .await
            .unwrap();
        migrate(&pool).await.expect("migrations must apply cleanly");
        Some(Self {
            admin,
            pool,
            schema,
        })
    }

    fn store(&self) -> Store {
        Store::from_pool(self.pool.clone())
    }

    async fn finish(self) {
        self.pool.close().await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {}_web_v1 CASCADE; DROP SCHEMA {} CASCADE",
            self.schema, self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }
}

/// A throwaway DATABASE (not schema) with a pinned session timezone, so
/// the session-timezone test cannot pass vacuously: every connection the
/// store pool opens inherits the database-level setting.
struct TimezoneFixture {
    admin: Pool<Postgres>,
    pool: Pool<Postgres>,
    database: String,
}

impl TimezoneFixture {
    async fn new(timezone: &str) -> Option<Self> {
        let url = database_url()?;
        let options = guarded_options(&url);
        let admin = two_bot_store::connect_pool_with_tls(
            &url,
            two_bot_core::database_tls::TlsPolicy::LocalOnly,
        )
        .await
        .expect("test DB connect failed");
        let tag: String = timezone
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect();
        let database = format!(
            "evr_tz_{}_{}_{tag}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {database}")))
            .execute(&admin)
            .await
            .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "ALTER DATABASE {database} SET timezone = '{timezone}'"
        )))
        .execute(&admin)
        .await
        .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(
                options
                    .database(&database)
                    .options([("statement_timeout", "15000ms".to_owned())]),
            )
            .await
            .unwrap();
        migrate(&pool).await.expect("migrations must apply cleanly");
        Some(Self {
            admin,
            pool,
            database,
        })
    }

    fn store(&self) -> Store {
        Store::from_pool(self.pool.clone())
    }

    async fn showed_timezone(&self) -> String {
        sqlx::query("SHOW timezone")
            .fetch_one(&self.pool)
            .await
            .expect("SHOW timezone must work")
            .try_get::<String, _>("TimeZone")
            .expect("SHOW timezone returns TimeZone")
    }

    async fn finish(self) {
        self.pool.close().await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {}",
            self.database
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }
}

#[test]
fn dst_probe_instants_are_self_consistent() {
    // T-15 is exactly 900s before each start.
    assert_eq!(A_START - A_T15, FIFTEEN_MINUTES);
    assert_eq!(B_START - B_T15, FIFTEEN_MINUTES);
    assert_eq!(C_START - C_T15, FIFTEEN_MINUTES);
    // The fall-back week is 169 hours long (the repeated 01:00 hour):
    // a fixed -04:00 computation fires B an hour early.
    assert_eq!(B_START - A_START, 7 * 86_400 + 3_600);
    assert_eq!(C_START - B_START, 7 * 86_400);
    assert_eq!((B_START - A_START) - (C_START - B_START), 3_600);
    // The spring-forward week is 167 hours long (the skipped 02:00 hour):
    // a fixed -05:00 computation fires D' an hour late.
    assert_eq!(DP_START - D_START, 7 * 86_400 - 3_600);
    assert_eq!(E_START - DP_START, 7 * 86_400);
    assert_eq!((E_START - DP_START) - (DP_START - D_START), 3_600);
    // The AC3 move is exactly one day later, inside the EST side.
    assert_eq!(BP_START - B_START, 86_400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrated_schema_parses_pinned_rfc3339_to_pinned_epochs() {
    let Some(fx) = Fixture::new().await else {
        return;
    };
    // PG's own timestamptz parser is the independent check on every
    // absolute instant pinned above: a wrong epoch fails here, loudly,
    // instead of silently shifting a window test.
    for (rfc3339, expected) in [
        ("2025-10-27T00:00:00Z", A_START),
        ("2025-10-26T23:45:00Z", A_T15),
        ("2025-11-03T01:00:00Z", B_START),
        ("2025-11-03T00:45:00Z", B_T15),
        ("2025-11-10T01:00:00Z", C_START),
        ("2025-11-10T00:45:00Z", C_T15),
        ("2025-03-03T01:00:00Z", D_START),
        ("2025-03-10T00:00:00Z", DP_START),
        ("2025-03-17T00:00:00Z", E_START),
        ("2025-11-04T01:00:00Z", BP_START),
    ] {
        let parsed: i64 =
            sqlx::query("SELECT EXTRACT(EPOCH FROM $1::timestamptz)::bigint AS epoch")
                .bind(rfc3339)
                .fetch_one(&fx.pool)
                .await
                .expect("epoch probe must run")
                .try_get::<i64, _>("epoch")
                .expect("epoch probe returns epoch");
        assert_eq!(parsed, expected, "pinned epoch for {rfc3339}");
    }
    fx.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn baseline_adapter_explicit_calls_report_not_implemented() {
    // Keep-alive for the [`BaselineEventVoice`] adapter (raid-pattern):
    // the Builder's inherent methods shadow the trait, so these explicit
    // qualified calls are what keep every trait method used in both
    // states. rustc warns per-method (`method X is never used`), so one
    // call is not enough for a four-method trait; without all four,
    // `clippy -D warnings` would fail GREEN and the Builder could not
    // fix it without editing this file. Passes identically before and
    // after GREEN by construction.
    let Some(fx) = Fixture::new().await else {
        return;
    };
    let store = fx.store();
    for error in [
        <Store as BaselineEventVoice>::sync_event_voice_series(&store, SERIES_FALL, &[])
            .await
            .expect_err("the sync baseline stub must error"),
        <Store as BaselineEventVoice>::due_event_voice_occurrences(&store, A_T15)
            .await
            .expect_err("the due baseline stub must error"),
        <Store as BaselineEventVoice>::read_event_voice_rows(&store, SERIES_FALL)
            .await
            .expect_err("the read baseline stub must error"),
        <Store as BaselineEventVoice>::bind_event_voice_channel(&store, 0, CHANNEL_A)
            .await
            .expect_err("the bind baseline stub must error"),
    ] {
        assert!(
            error.to_string().contains("not implemented"),
            "stub error must say not implemented: {error}"
        );
    }
    fx.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn weekly_series_spanning_fall_back_fires_each_occurrence_at_its_own_t15() {
    let Some(fx) = Fixture::new().await else {
        return;
    };
    let store = fx.store();
    let ids = store
        .sync_event_voice_series(
            SERIES_FALL,
            &[scheduled(A_START), scheduled(B_START), scheduled(C_START)],
        )
        .await
        .expect("syncing the fall series must succeed");
    assert_eq!(ids.len(), 3, "one row id per occurrence, in order");
    assert_eq!(
        store
            .due_event_voice_occurrences(A_T15)
            .await
            .expect("due query at A T-15 must succeed"),
        vec![ids[0]],
        "at A's T-15 only A is due: B and C are days out"
    );
    assert_eq!(
        store
            .due_event_voice_occurrences(B_T15)
            .await
            .expect("due query at B T-15 must succeed"),
        vec![ids[1]],
        "at B's T-15 only B is due: B fires at 00:45Z (EST), not 23:45Z"
    );
    assert_eq!(
        store
            .due_event_voice_occurrences(C_T15)
            .await
            .expect("due query at C T-15 must succeed"),
        vec![ids[2]],
        "at C's T-15 only C is due"
    );
    assert_eq!(
        store
            .due_event_voice_occurrences(A_T15 - 1)
            .await
            .expect("due query one second before A T-15 must succeed"),
        Vec::<i64>::new(),
        "one second before the window opens nothing is due"
    );
    fx.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn est_to_edt_shift_updates_the_single_row_and_keeps_its_channel() {
    let Some(fx) = Fixture::new().await else {
        return;
    };
    let store = fx.store();
    assert!(
        store
            .read_event_voice_rows(SERIES_SPRING)
            .await
            .expect("reading a fresh series must succeed")
            .is_empty(),
        "a fresh series tracks no rows"
    );
    let before = store
        .sync_event_voice_series(SERIES_SPRING, &[occ(D_START, None, EventStatus::Scheduled)])
        .await
        .expect("syncing the EST occurrence must succeed");
    assert_eq!(before.len(), 1);
    store
        .bind_event_voice_channel(before[0], CHANNEL_D)
        .await
        .expect("binding the created channel must succeed");
    // Discord rewrites the event start across the spring-forward
    // transition: the mirror must UPDATE the tracked row, never insert a
    // second row that would orphan the live channel.
    let after = store
        .sync_event_voice_series(
            SERIES_SPRING,
            &[occ(
                DP_START,
                Some(DP_START + TWO_HOURS),
                EventStatus::Scheduled,
            )],
        )
        .await
        .expect("syncing the shifted EDT occurrence must succeed");
    assert_eq!(
        after, before,
        "a starts_at shift resolves to the same row id (update, not duplicate)"
    );
    let rows = store
        .read_event_voice_rows(SERIES_SPRING)
        .await
        .expect("reading the shifted series must succeed");
    assert_eq!(rows.len(), 1, "the shift must not duplicate the row");
    let (row_id, starts_at, ends_at, channel, status) = &rows[0];
    assert_eq!(*row_id, before[0]);
    assert_eq!(*starts_at, DP_START, "the row carries the new start");
    assert_eq!(
        *ends_at,
        Some(DP_START + TWO_HOURS),
        "the row carries the new end"
    );
    assert_eq!(
        channel.as_deref(),
        Some(CHANNEL_D),
        "the shift keeps the live channel"
    );
    assert_eq!(status, EventStatus::Scheduled.as_str());
    fx.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_occurrence_start_inserts_a_new_row() {
    let Some(fx) = Fixture::new().await else {
        return;
    };
    let store = fx.store();
    let first = store
        .sync_event_voice_series(SERIES_SPRING, &[scheduled(D_START)])
        .await
        .expect("syncing the first occurrence must succeed");
    assert_eq!(first.len(), 1);
    let both = store
        .sync_event_voice_series(SERIES_SPRING, &[scheduled(DP_START), scheduled(E_START)])
        .await
        .expect("syncing with a new occurrence must succeed");
    assert_eq!(both.len(), 2, "one row id per occurrence, in order");
    // The spring-forward shift of D resolves to the existing row while
    // the genuinely new E start inserts: update and insert in one sync.
    assert_eq!(both[0], first[0], "the shifted occurrence keeps its row");
    assert_ne!(both[1], first[0], "the new occurrence gets a new row");
    let rows = store
        .read_event_voice_rows(SERIES_SPRING)
        .await
        .expect("reading the grown series must succeed");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, first[0]);
    assert_eq!(rows[0].1, DP_START);
    assert_eq!(rows[1].0, both[1]);
    assert_eq!(rows[1].1, E_START);
    fx.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moved_instance_keeps_channel_cancelled_row_closes_siblings_untouched() {
    let Some(fx) = Fixture::new().await else {
        return;
    };
    let store = fx.store();
    let ids = store
        .sync_event_voice_series(
            SERIES_FALL,
            &[scheduled(A_START), scheduled(B_START), scheduled(C_START)],
        )
        .await
        .expect("syncing the fall series must succeed");
    assert_eq!(ids.len(), 3);
    for (row_id, channel) in [
        (ids[0], CHANNEL_A),
        (ids[1], CHANNEL_B),
        (ids[2], CHANNEL_C),
    ] {
        store
            .bind_event_voice_channel(row_id, channel)
            .await
            .expect("binding channels must succeed");
    }
    let snapshot = store
        .read_event_voice_rows(SERIES_FALL)
        .await
        .expect("reading the bound series must succeed");
    assert_eq!(snapshot.len(), 3);
    // One instance moves a day later, one is cancelled per-occurrence:
    // the move keeps its row and channel, the cancel closes only its
    // own row, and the untouched sibling is byte-identical.
    let resolved = store
        .sync_event_voice_series(
            SERIES_FALL,
            &[
                scheduled(A_START),
                scheduled(BP_START),
                occ(C_START, Some(C_START + TWO_HOURS), EventStatus::Cancelled),
            ],
        )
        .await
        .expect("syncing the move plus cancel must succeed");
    assert_eq!(resolved, ids, "every occurrence resolves to its own row");
    let rows = store
        .read_event_voice_rows(SERIES_FALL)
        .await
        .expect("reading the resolved series must succeed");
    assert_eq!(rows.len(), 3, "no row is added or lost");
    assert_eq!(
        rows[0], snapshot[0],
        "the untouched sibling is byte-identical"
    );
    assert_eq!(rows[1].0, ids[1], "the moved instance keeps its row");
    assert_eq!(
        rows[1].1, BP_START,
        "the moved instance carries its new start"
    );
    assert_eq!(
        rows[1].3.as_deref(),
        Some(CHANNEL_B),
        "the moved instance keeps its channel"
    );
    assert_eq!(rows[1].4, EventStatus::Scheduled.as_str());
    assert_eq!(rows[2].0, ids[2], "the cancelled instance keeps its row");
    assert_eq!(rows[2].1, C_START);
    assert_eq!(
        rows[2].4,
        EventStatus::Cancelled.as_str(),
        "the cancelled row closes"
    );
    assert_eq!(
        rows[2].3.as_deref(),
        Some(CHANNEL_C),
        "keying never drops the channel: lifecycle is the ticker's job"
    );
    fx.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn due_window_is_identical_under_any_session_timezone() {
    let Some(utc) = TimezoneFixture::new("UTC").await else {
        return;
    };
    let Some(new_york) = TimezoneFixture::new("America/New_York").await else {
        utc.finish().await;
        return;
    };
    let Some(kiritimati) = TimezoneFixture::new("Pacific/Kiritimati").await else {
        utc.finish().await;
        new_york.finish().await;
        return;
    };
    assert_eq!(utc.showed_timezone().await, "UTC");
    assert_eq!(new_york.showed_timezone().await, "America/New_York");
    assert_eq!(kiritimati.showed_timezone().await, "Pacific/Kiritimati");
    // The same series under three session zones: timestamptz window
    // maths in the DB cannot tell them apart, while any local-time
    // rendering in the query path shifts the due set by hours.
    for fx in [&utc, &new_york, &kiritimati] {
        fx.store()
            .sync_event_voice_series(
                SERIES_FALL,
                &[scheduled(A_START), scheduled(B_START), scheduled(C_START)],
            )
            .await
            .expect("syncing the fall series must succeed");
    }
    let mut due_sets = Vec::new();
    for (name, fx) in [
        ("UTC", &utc),
        ("America/New_York", &new_york),
        ("Pacific/Kiritimati", &kiritimati),
    ] {
        let due = fx
            .store()
            .due_event_voice_occurrences(B_T15)
            .await
            .expect("due query under {name} must succeed");
        assert_eq!(due.len(), 1, "exactly B is due at B T-15 under {name}");
        let rows = fx
            .store()
            .read_event_voice_rows(SERIES_FALL)
            .await
            .expect("reading the series must succeed");
        let starts: Vec<i64> = rows
            .iter()
            .filter(|row| due.contains(&row.0))
            .map(|row| row.1)
            .collect();
        due_sets.push(starts);
    }
    assert_eq!(due_sets[0], vec![B_START]);
    assert_eq!(
        due_sets[1], due_sets[0],
        "the due set under America/New_York must equal UTC"
    );
    assert_eq!(
        due_sets[2], due_sets[0],
        "the due set under Pacific/Kiritimati must equal UTC"
    );
    utc.finish().await;
    new_york.finish().await;
    kiritimati.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn due_window_matches_r1_half_open_interval_and_eligibility() {
    let Some(fx) = Fixture::new().await else {
        return;
    };
    let store = fx.store();
    let now = A_T15;
    // R1: starts_at <= now + 15m AND starts_at > now - 5m.
    let in_near_start = now - FIVE_MINUTES + 1;
    let out_just_started = now - FIVE_MINUTES;
    let in_far_edge = now + FIFTEEN_MINUTES;
    let out_just_late = now + FIFTEEN_MINUTES + 1;
    let in_other_event = now + 60;
    let in_but_bound = now + 120;
    let in_but_cancelled = now + 180;
    let ids = store
        .sync_event_voice_series(
            SERIES_FALL,
            &[
                scheduled(in_near_start),
                scheduled(out_just_started),
                scheduled(in_far_edge),
                scheduled(out_just_late),
                scheduled(in_but_bound),
                occ(
                    in_but_cancelled,
                    Some(in_but_cancelled + TWO_HOURS),
                    EventStatus::Cancelled,
                ),
            ],
        )
        .await
        .expect("syncing the boundary series must succeed");
    assert_eq!(ids.len(), 6);
    store
        .bind_event_voice_channel(ids[4], CHANNEL_A)
        .await
        .expect("binding a channel must succeed");
    let other = store
        .sync_event_voice_series(SERIES_OTHER, &[scheduled(in_other_event)])
        .await
        .expect("syncing the other event must succeed");
    assert_eq!(other.len(), 1);
    // starts_at ascending: the near-start row, the other event's row,
    // then the far-edge row at exactly now + 15m.
    assert_eq!(
        store
            .due_event_voice_occurrences(now)
            .await
            .expect("due query must succeed"),
        vec![ids[0], other[0], ids[2]],
        "due = (now-5m, now+15m] intersect scheduled intersect channel-less"
    );
    fx.finish().await;
}
