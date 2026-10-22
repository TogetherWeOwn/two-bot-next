//! Acceptance tests: self-role ticket-row acknowledgement columns.
//!
//! Protected test file: the build card implements against these tests and
//! must not edit them. If a pinned name below cannot be implemented as
//! written, stop and escalate instead of changing the tests.
//!
//! Contract pinned here (build card implements exactly this):
//!
//! - A new migration in `crates/cutover/migrations/` named
//!   `0449_self_role_ticket_acknowledgement.sql` adds five columns to the
//!   existing `self_role_exchanges` ticket table (created by `0205`, retired
//!   shape by `0206`): `settled_at TIMESTAMPTZ NULL`, `settled_as TEXT NULL`
//!   limited to `superseded_settled` / `expired_unknown`, `superseded_by TEXT
//!   NULL` (replacement ticket id), `attempt_no SMALLINT NOT NULL DEFAULT 1`
//!   with `attempt_no >= 1`, and `outcome_class TEXT NULL` limited to the
//!   three ambiguous values `timeout_no_response` / `response_ambiguous` /
//!   `loop_exhaustion` (NULL for definite outcomes). The migration is
//!   additive and idempotent (`ADD COLUMN IF NOT EXISTS`, guarded CHECK
//!   constraints); its rollback is `DROP COLUMN` for the five columns and is
//!   documented in the file header. Numbering note: `0447` is already taken
//!   globally by the assistant-ledger cutover mirror, and `0448` by the
//!   stop-reason alter, so the next free global migration number is `0449`
//!   (the numbering gate rejects duplicates across chains).
//! - `SelfRoleStore::check_acknowledgement_caps(guild_unresolved: u32,
//!   member_unresolved: u32, event_unresolved: u32, row_bytes: usize) ->
//!   Result<(), E>` is a pure associated function (no database): it allows
//!   the insert only when every count is strictly under its cap (500
//!   rows/guild, 50/member, 20/event) and `row_bytes <= 512`. Over any cap it
//!   returns `Err` whose `Display` names the dimension (`guild`, `member`,
//!   `event`, `row`/`byte`/`512`) and contains `refus` (operator-visible
//!   refusal, never silent). The error type is the builder's choice but must
//!   implement `Debug + Display`.
//! - `SelfRoleStore::journal_role_exchange` (existing API, unchanged
//!   signature) consults the caps before inserting: when the guild, member
//!   or event already holds its maximum of unresolved rows, the call returns
//!   `Err` (never `Ok(None)`, which keeps meaning not-owned) and inserts
//!   nothing. A refused attempt never partially writes.
//! - Ambiguous outcomes are representable per send through `outcome_class`;
//!   definite completions via the existing `complete_role_exchange` keep
//!   working and their `outcome_class` is never one of the ambiguous three.
//!   Pre-existing rows read back with `attempt_no = 1` and the four nullable
//!   columns NULL.
//!
//! Test shapes: offline unit tests (no database, run by default) pin the
//! pure cap helper through a baseline adapter trait that always allows; the
//! builder's inherent associated function shadows it. Database tests
//! (`#[ignore]`, disposable agent-testdb schema only) pin the migration
//! apply/rollback round-trip, the column defaults, the journal wiring and
//! the outcome representation through direct SQL plus the existing store
//! APIs. No live Discord, no staging or production database.
//!
//! CI routing note for the builder: this file contains `#[ignore]` targets,
//! so the test-inventory gate requires an explicit `--test
//! self_role_ack_acceptance` step (the self-role-store job is the natural
//! home). Add it in the implementation PR alongside this file.

use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use two_bot_core::{PanelMode, RoleOperation, SettledOutcome};
use two_bot_cutover::self_role_store::{
    AuditEffects, ExchangeIntent, ExchangeReceipt, SelfRoleAudit, SelfRoleStore,
};

// ---------------------------------------------------------------------------
// Pure cap helper: baseline adapter (offline, no database)
// ---------------------------------------------------------------------------

/// Baseline shape of the acknowledgement cap check. At baseline there is no
/// cap enforcement, so the adapter always allows; every over-cap test below
/// fails until the builder adds the inherent associated function, which
/// shadows these unqualified calls. The error type stays generic (`String`
/// here, the builder's refusal type later) so the same assertions compile in
/// both states; only `is_ok` / `is_err` / `Display` are relied upon.
trait BaselineAckCaps {
    fn check_acknowledgement_caps(
        guild_unresolved: u32,
        member_unresolved: u32,
        event_unresolved: u32,
        row_bytes: usize,
    ) -> Result<(), String>;
}

impl BaselineAckCaps for SelfRoleStore {
    fn check_acknowledgement_caps(
        guild_unresolved: u32,
        member_unresolved: u32,
        event_unresolved: u32,
        row_bytes: usize,
    ) -> Result<(), String> {
        let _ = (
            guild_unresolved,
            member_unresolved,
            event_unresolved,
            row_bytes,
        );
        Ok(())
    }
}

fn refusal_message(result: Result<(), impl ToString + std::fmt::Debug>, what: &str) -> String {
    let err = result.expect_err(what);
    err.to_string()
}

#[test]
fn ack_caps_empty_counts_allow() {
    let result = SelfRoleStore::check_acknowledgement_caps(0, 0, 0, 100);
    assert!(result.is_ok(), "empty counts must allow: {result:?}");
}

#[test]
fn ack_caps_all_just_under_limits_allow() {
    let result = SelfRoleStore::check_acknowledgement_caps(499, 49, 19, 512);
    assert!(
        result.is_ok(),
        "just-under caps with a 512 B row must allow: {result:?}"
    );
}

#[test]
fn ack_caps_row_bound_boundary_allows() {
    let result = SelfRoleStore::check_acknowledgement_caps(0, 0, 0, 512);
    assert!(result.is_ok(), "exactly 512 B must allow: {result:?}");
}

#[test]
fn ack_caps_guild_cap_refuses() {
    let result = SelfRoleStore::check_acknowledgement_caps(500, 0, 0, 100);
    let msg = refusal_message(result, "500 guild rows must refuse the 501st");
    assert!(msg.contains("guild"), "refusal must name guild: {msg}");
    assert!(
        msg.to_ascii_lowercase().contains("refus"),
        "operator-visible refusal, not silent substitution: {msg}"
    );
}

#[test]
fn ack_caps_member_cap_refuses() {
    let result = SelfRoleStore::check_acknowledgement_caps(0, 50, 0, 100);
    let msg = refusal_message(result, "50 member rows must refuse the 51st");
    assert!(msg.contains("member"), "refusal must name member: {msg}");
    assert!(
        msg.to_ascii_lowercase().contains("refus"),
        "operator-visible refusal, not silent substitution: {msg}"
    );
}

#[test]
fn ack_caps_event_cap_refuses() {
    let result = SelfRoleStore::check_acknowledgement_caps(0, 0, 20, 100);
    let msg = refusal_message(result, "20 event rows must refuse the 21st");
    assert!(msg.contains("event"), "refusal must name event: {msg}");
    assert!(
        msg.to_ascii_lowercase().contains("refus"),
        "operator-visible refusal, not silent substitution: {msg}"
    );
}

#[test]
fn ack_caps_row_bound_refuses() {
    let result = SelfRoleStore::check_acknowledgement_caps(0, 0, 0, 513);
    let msg = refusal_message(result, "a 513 B row must be refused");
    assert!(
        msg.contains("row") || msg.contains("byte") || msg.contains("512"),
        "refusal must name the row bound: {msg}"
    );
    assert!(
        msg.to_ascii_lowercase().contains("refus"),
        "operator-visible refusal, not silent substitution: {msg}"
    );
}

#[test]
fn ack_caps_baseline_trait_stays_live() {
    // Explicit trait path: keeps BaselineAckCaps used after the builder's
    // inherent method shadows the unqualified calls above, so clippy
    // `-D warnings` stays green in both states.
    let baseline: Result<(), String> =
        <SelfRoleStore as BaselineAckCaps>::check_acknowledgement_caps(0, 0, 0, 100);
    assert!(baseline.is_ok(), "baseline adapter always allows");
}

// ---------------------------------------------------------------------------
// Migration file contract (offline: filesystem only, no database)
// ---------------------------------------------------------------------------

const ACK_MIGRATION_FILE: &str = "0449_self_role_ticket_acknowledgement.sql";

const ACK_COLUMNS: [&str; 5] = [
    "settled_at",
    "settled_as",
    "superseded_by",
    "attempt_no",
    "outcome_class",
];

const AMBIGUOUS_OUTCOMES: [&str; 3] = [
    "timeout_no_response",
    "response_ambiguous",
    "loop_exhaustion",
];

fn ack_migration_path() -> String {
    format!(
        "{}/migrations/{ACK_MIGRATION_FILE}",
        env!("CARGO_MANIFEST_DIR")
    )
}

#[test]
fn ack_migration_file_exists_and_pins_expected_columns() {
    let path = ack_migration_path();
    assert!(
        std::path::Path::new(&path).exists(),
        "acknowledgement migration must exist at {path}"
    );
    let content = std::fs::read_to_string(&path).expect("migration file must be readable");
    for pinned in ACK_COLUMNS {
        assert!(
            content.contains(pinned),
            "migration must add column {pinned}"
        );
    }
    for outcome in AMBIGUOUS_OUTCOMES {
        assert!(
            content.contains(outcome),
            "migration must admit ambiguous outcome {outcome}"
        );
    }
    assert!(
        content.to_ascii_lowercase().contains("rollback") || content.contains("DROP COLUMN"),
        "migration must document its rollback (DROP COLUMN)"
    );
}

// ---------------------------------------------------------------------------
// Disposable-database fixture (agent-testdb only, isolated schema per test)
// ---------------------------------------------------------------------------

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const BASE_MIGRATIONS: [&str; 7] = [
    include_str!("../migrations/0200_self_roles.sql"),
    include_str!("../migrations/0201_self_role_intent_initialization.sql"),
    include_str!("../migrations/0202_self_role_compensation_phase.sql"),
    include_str!("../migrations/0203_self_role_pending_exchange.sql"),
    include_str!("../migrations/0204_self_role_terminal_repair.sql"),
    include_str!("../migrations/0205_self_role_exchange_receipts.sql"),
    include_str!("../migrations/0206_self_role_exchange_baselines.sql"),
];

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

/// Disposable schema name: process id, a process-wide monotonic counter, and
/// a clock sample. The counter (not the clock) is the uniqueness guarantee
/// under parallel execution: tests in one process share the pid and can sample
/// the same nanos tick, which used to collide on `CREATE SCHEMA` (`23505` on
/// `pg_namespace_nspname_index`).
fn schema_name(clock_sample: u128) -> String {
    format!(
        "self_role_ack_{}_{}_{}",
        std::process::id(),
        NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed),
        clock_sample
    )
}

#[test]
fn schema_names_are_distinct_when_the_clock_repeats() {
    assert_ne!(schema_name(42), schema_name(42));
}

/// One generated schema per scenario, dropped even when the scenario panics.
async fn isolated<F, Fut>(scenario: F) -> TestResult
where
    F: FnOnce(PgPool) -> Fut,
    Fut: Future<Output = TestResult> + Send + 'static,
{
    let options = PgConnectOptions::new()
        .host("agent-testdb")
        .port(5432)
        .username("agent_test")
        .password("")
        .database("agent_test");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await?;
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    // Fixed prefix plus numeric PID/counter/timestamp; no caller-supplied identifiers.
    let schema = schema_name(now_nanos);
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await?;
    let search_path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .after_connect(move |conn, _| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(search_path)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await?;
    for migration in BASE_MIGRATIONS {
        sqlx::raw_sql(migration).execute(&pool).await?;
    }
    let outcome = tokio::spawn(scenario(pool.clone())).await;
    pool.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await?;
    admin.close().await;
    match outcome {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(Box::new(error)),
    }
}

async fn apply_ack_migration(pool: &PgPool) -> TestResult {
    let path = ack_migration_path();
    assert!(
        std::path::Path::new(&path).exists(),
        "acknowledgement migration must exist at {path}"
    );
    let content = std::fs::read_to_string(&path)?;
    // Audited: the file is the reviewed acknowledgement migration; applying
    // it inside a disposable test schema is this test's purpose.
    sqlx::raw_sql(sqlx::AssertSqlSafe(content))
        .execute(pool)
        .await?;
    Ok(())
}

async fn exchange_columns(pool: &PgPool) -> TestResult<HashSet<String>> {
    let rows = sqlx::query(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = current_schema() \
         AND table_name = 'self_role_exchanges'",
    )
    .fetch_all(pool)
    .await?;
    let mut columns = HashSet::new();
    for row in &rows {
        columns.insert(row.try_get::<String, _>("column_name")?);
    }
    Ok(columns)
}

fn audit_row(event_id: &str, guild_id: &str, member_id: &str) -> SelfRoleAudit {
    SelfRoleAudit {
        event_id: event_id.to_owned(),
        event_order: Some(event_id.to_owned()),
        guild_id: guild_id.to_owned(),
        member_id: member_id.to_owned(),
        panel_id: "test-panel".to_owned(),
        source_id: "test-message".to_owned(),
        option_key: Some("test-option".to_owned()),
        role_id: Some("101".to_owned()),
        source: PanelMode::Button,
        operation: RoleOperation::Replace,
        outcome: SettledOutcome::Switched,
        code: None,
        reason: None,
        effects: AuditEffects::default(),
        desired_role_ids: Vec::new(),
        pre_mutation_role_ids: Vec::new(),
    }
}

fn intent_for(role_id: &str) -> ExchangeIntent {
    ExchangeIntent {
        role_id: role_id.to_owned(),
        adding: true,
        compensating: false,
    }
}

async fn pending_exchanges_for_guild(pool: &PgPool, guild_id: &str) -> TestResult<i64> {
    let (count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM self_role_exchanges e \
         JOIN self_role_audit a ON e.event_id = a.event_id \
         WHERE a.guild_id = $1 AND e.disposition = 'pending'",
    )
    .bind(guild_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// Database tests: migration round-trip, defaults, caps, outcome representation
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn ack_migration_applies_and_preexisting_rows_get_sane_defaults() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        let audit = audit_row("preexisting-event", "guild-1", "member-1");
        let claim = store
            .claim_audit(&audit)
            .await?
            .expect("fresh audit must claim");
        store
            .journal_role_exchange(&claim, None, &intent_for("101"))
            .await?
            .expect("baseline journal must succeed");
        apply_ack_migration(&pool).await?;
        let columns = exchange_columns(&pool).await?;
        for pinned in ACK_COLUMNS {
            assert!(
                columns.contains(pinned),
                "missing column {pinned}: {columns:?}"
            );
        }
        let row = sqlx::query(
            "SELECT attempt_no, settled_at::text AS settled_at, settled_as, \
             superseded_by, outcome_class \
             FROM self_role_exchanges WHERE event_id = 'preexisting-event'",
        )
        .fetch_one(&pool)
        .await?;
        let attempt: i16 = row.try_get("attempt_no")?;
        assert_eq!(attempt, 1, "pre-existing rows represent one send");
        let settled_at: Option<String> = row.try_get("settled_at")?;
        assert!(settled_at.is_none(), "pre-existing rows start unsettled");
        let settled_as: Option<String> = row.try_get("settled_as")?;
        assert!(settled_as.is_none());
        let superseded_by: Option<String> = row.try_get("superseded_by")?;
        assert!(superseded_by.is_none());
        let outcome: Option<String> = row.try_get("outcome_class")?;
        assert!(outcome.is_none(), "pre-existing rows carry no outcome yet");
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn ack_migration_is_idempotent_and_rolls_back_cleanly() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        let audit = audit_row("rollback-event", "guild-1", "member-1");
        let claim = store
            .claim_audit(&audit)
            .await?
            .expect("fresh audit must claim");
        store
            .journal_role_exchange(&claim, None, &intent_for("101"))
            .await?
            .expect("baseline journal must succeed");
        apply_ack_migration(&pool).await?;
        // Re-applying the whole acknowledgement migration is a no-op.
        apply_ack_migration(&pool).await?;
        let columns = exchange_columns(&pool).await?;
        assert!(columns.contains("attempt_no"));
        // Rollback drops exactly the acknowledgement columns.
        sqlx::raw_sql(
            "ALTER TABLE self_role_exchanges DROP COLUMN IF EXISTS settled_at, \
             DROP COLUMN IF EXISTS settled_as, DROP COLUMN IF EXISTS superseded_by, \
             DROP COLUMN IF EXISTS attempt_no, DROP COLUMN IF EXISTS outcome_class",
        )
        .execute(&pool)
        .await?;
        let columns = exchange_columns(&pool).await?;
        for pinned in ACK_COLUMNS {
            assert!(!columns.contains(pinned), "rollback must drop {pinned}");
        }
        // The pre-existing ticket survives the round-trip.
        let (disposition,): (String,) = sqlx::query_as(
            "SELECT disposition FROM self_role_exchanges WHERE event_id = 'rollback-event'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(disposition, "pending");
        // Re-applying restores the columns with the same defaults.
        apply_ack_migration(&pool).await?;
        let columns = exchange_columns(&pool).await?;
        for pinned in ACK_COLUMNS {
            assert!(columns.contains(pinned), "re-apply must restore {pinned}");
        }
        let (attempt,): (i16,) = sqlx::query_as(
            "SELECT attempt_no FROM self_role_exchanges WHERE event_id = 'rollback-event'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(attempt, 1);
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn ack_guild_cap_refuses_before_insert_without_partial_write() -> TestResult {
    isolated(|pool| async move {
        apply_ack_migration(&pool).await?;
        let store = SelfRoleStore::new(pool.clone());
        // 500 pending rows in one guild, spread over 500 members/events so no
        // member or event cap trips first.
        for n in 0..500 {
            let event = format!("guild-cap-event-{n}");
            let member = format!("guild-cap-member-{n}");
            let audit = audit_row(&event, "guild-cap", &member);
            store.claim_audit(&audit).await?.expect("setup claim");
            sqlx::query(
                "INSERT INTO self_role_exchanges \
                 (event_id, origin_generation, role_id, adding, compensating) \
                 VALUES ($1, 1, '101', TRUE, FALSE)",
            )
            .bind(&event)
            .execute(&pool)
            .await?;
        }
        assert_eq!(pending_exchanges_for_guild(&pool, "guild-cap").await?, 500);
        let audit = audit_row("guild-cap-event-new", "guild-cap", "guild-cap-member-new");
        let claim = store
            .claim_audit(&audit)
            .await?
            .expect("fresh audit must claim");
        let result = store
            .journal_role_exchange(&claim, None, &intent_for("101"))
            .await;
        assert!(
            result.is_err(),
            "the 501st guild row must be refused, got {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("guild"), "refusal must name guild: {msg}");
        assert!(
            msg.to_ascii_lowercase().contains("refus"),
            "operator-visible refusal: {msg}"
        );
        assert_eq!(
            pending_exchanges_for_guild(&pool, "guild-cap").await?,
            500,
            "a refused attempt must never partially write"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn ack_member_cap_refuses_before_insert() -> TestResult {
    isolated(|pool| async move {
        apply_ack_migration(&pool).await?;
        let store = SelfRoleStore::new(pool.clone());
        // 50 pending rows for one member across 50 events.
        for n in 0..50 {
            let event = format!("member-cap-event-{n}");
            let audit = audit_row(&event, "guild-1", "member-cap");
            store.claim_audit(&audit).await?.expect("setup claim");
            sqlx::query(
                "INSERT INTO self_role_exchanges \
                 (event_id, origin_generation, role_id, adding, compensating) \
                 VALUES ($1, 1, '101', TRUE, FALSE)",
            )
            .bind(&event)
            .execute(&pool)
            .await?;
        }
        let audit = audit_row("member-cap-event-new", "guild-1", "member-cap");
        let claim = store
            .claim_audit(&audit)
            .await?
            .expect("fresh audit must claim");
        let result = store
            .journal_role_exchange(&claim, None, &intent_for("101"))
            .await;
        assert!(
            result.is_err(),
            "the 51st member row must be refused, got {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("member"), "refusal must name member: {msg}");
        let (count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM self_role_exchanges e \
             JOIN self_role_audit a ON e.event_id = a.event_id \
             WHERE a.guild_id = 'guild-1' AND a.member_id = 'member-cap' \
             AND e.disposition = 'pending'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(count, 50, "a refused attempt must never partially write");
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn ack_event_cap_refuses_before_insert() -> TestResult {
    isolated(|pool| async move {
        apply_ack_migration(&pool).await?;
        let store = SelfRoleStore::new(pool.clone());
        let audit = audit_row("event-cap", "guild-1", "member-1");
        let claim = store
            .claim_audit(&audit)
            .await?
            .expect("fresh audit must claim");
        // 20 pending rows for one event across 20 distinct roles.
        for n in 101..121 {
            let role = n.to_string();
            sqlx::query(
                "INSERT INTO self_role_exchanges \
                 (event_id, origin_generation, role_id, adding, compensating) \
                 VALUES ('event-cap', 1, $1, TRUE, FALSE)",
            )
            .bind(&role)
            .execute(&pool)
            .await?;
        }
        let result = store
            .journal_role_exchange(&claim, None, &intent_for("121"))
            .await;
        assert!(
            result.is_err(),
            "the 21st event row must be refused, got {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("event"), "refusal must name event: {msg}");
        let (count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM self_role_exchanges \
             WHERE event_id = 'event-cap' AND disposition = 'pending'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(count, 20, "a refused attempt must never partially write");
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn ack_max_row_fits_within_512_bytes() -> TestResult {
    isolated(|pool| async move {
        apply_ack_migration(&pool).await?;
        let columns = exchange_columns(&pool).await?;
        assert!(
            columns.contains("outcome_class"),
            "outcome column must exist before sizing"
        );
        let store = SelfRoleStore::new(pool.clone());
        let audit = audit_row("sized-event", "guild-1", "member-1");
        let claim = store
            .claim_audit(&audit)
            .await?
            .expect("fresh audit must claim");
        // Longest valid snowflake (20 digits) plus the longest enum spellings.
        store
            .journal_role_exchange(&claim, None, &intent_for("18446744073709551615"))
            .await?
            .expect("max-size journal must succeed under the caps");
        sqlx::query(
            "UPDATE self_role_exchanges SET outcome_class = 'timeout_no_response', \
             settled_as = 'superseded_settled', superseded_by = 'replacement-ticket' \
             WHERE event_id = 'sized-event'",
        )
        .execute(&pool)
        .await?;
        let (size,): (i32,) = sqlx::query_as(
            "SELECT pg_column_size(self_role_exchanges) FROM self_role_exchanges \
             WHERE event_id = 'sized-event'",
        )
        .fetch_one(&pool)
        .await?;
        assert!(
            size <= 512,
            "a max-size acknowledgement row must fit 512 B, got {size}"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn ack_ambiguous_outcomes_round_trip_and_differ_from_definite() -> TestResult {
    isolated(|pool| async move {
        apply_ack_migration(&pool).await?;
        let columns = exchange_columns(&pool).await?;
        assert!(
            columns.contains("outcome_class"),
            "outcome column must exist before representing outcomes"
        );
        let store = SelfRoleStore::new(pool.clone());
        // Each ambiguous class persists per send.
        for (n, outcome) in AMBIGUOUS_OUTCOMES.iter().enumerate() {
            let event = format!("ambiguous-event-{n}");
            let audit = audit_row(&event, "guild-1", "member-1");
            let claim = store
                .claim_audit(&audit)
                .await?
                .expect("fresh audit must claim");
            store
                .journal_role_exchange(&claim, None, &intent_for("101"))
                .await?
                .expect("journal must succeed");
            sqlx::query("UPDATE self_role_exchanges SET outcome_class = $1 WHERE event_id = $2")
                .bind(outcome)
                .bind(&event)
                .execute(&pool)
                .await?;
            let (stored,): (Option<String>,) =
                sqlx::query_as("SELECT outcome_class FROM self_role_exchanges WHERE event_id = $1")
                    .bind(&event)
                    .fetch_one(&pool)
                    .await?;
            assert_eq!(stored.as_deref(), Some(*outcome));
        }
        // A definite completion is never one of the ambiguous three.
        let audit = audit_row("definite-event", "guild-1", "member-1");
        let claim = store
            .claim_audit(&audit)
            .await?
            .expect("fresh audit must claim");
        let ticket = store
            .journal_role_exchange(&claim, None, &intent_for("101"))
            .await?
            .expect("journal must succeed");
        assert!(
            store
                .complete_role_exchange(&ticket, ExchangeReceipt::Response { status: 204 })
                .await?,
            "definite completion must still apply"
        );
        let (stored,): (Option<String>,) = sqlx::query_as(
            "SELECT outcome_class FROM self_role_exchanges WHERE event_id = 'definite-event'",
        )
        .fetch_one(&pool)
        .await?;
        assert!(
            stored.is_none() || !AMBIGUOUS_OUTCOMES.contains(&stored.as_deref().unwrap_or("")),
            "definite outcomes must differ from ambiguous classes, got {stored:?}"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn ack_invalid_outcome_settled_and_attempt_values_are_rejected() -> TestResult {
    isolated(|pool| async move {
        apply_ack_migration(&pool).await?;
        let columns = exchange_columns(&pool).await?;
        assert!(
            columns.contains("outcome_class"),
            "outcome column must exist before testing its constraint"
        );
        let store = SelfRoleStore::new(pool.clone());
        let audit = audit_row("constraint-event", "guild-1", "member-1");
        let claim = store
            .claim_audit(&audit)
            .await?
            .expect("fresh audit must claim");
        store
            .journal_role_exchange(&claim, None, &intent_for("101"))
            .await?
            .expect("journal must succeed");
        let bad_outcome = sqlx::query(
            "UPDATE self_role_exchanges SET outcome_class = 'bogus' \
             WHERE event_id = 'constraint-event'",
        )
        .execute(&pool)
        .await;
        assert!(
            bad_outcome.is_err(),
            "unknown outcome classes must be rejected"
        );
        let bad_settled = sqlx::query(
            "UPDATE self_role_exchanges SET settled_as = 'applied' \
             WHERE event_id = 'constraint-event'",
        )
        .execute(&pool)
        .await;
        assert!(
            bad_settled.is_err(),
            "ambiguous sends must never settle as applied"
        );
        let bad_attempt = sqlx::query(
            "UPDATE self_role_exchanges SET attempt_no = 0 WHERE event_id = 'constraint-event'",
        )
        .execute(&pool)
        .await;
        assert!(
            bad_attempt.is_err(),
            "attempt numbers below one must be rejected"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn ack_new_journals_carry_attempt_one_and_null_settlement() -> TestResult {
    isolated(|pool| async move {
        apply_ack_migration(&pool).await?;
        let columns = exchange_columns(&pool).await?;
        assert!(
            columns.contains("attempt_no"),
            "attempt column must exist before reading journals"
        );
        let store = SelfRoleStore::new(pool.clone());
        let audit = audit_row("journal-defaults", "guild-1", "member-1");
        let claim = store
            .claim_audit(&audit)
            .await?
            .expect("fresh audit must claim");
        store
            .journal_role_exchange(&claim, None, &intent_for("101"))
            .await?
            .expect("journal must succeed");
        let row = sqlx::query(
            "SELECT attempt_no, settled_at::text AS settled_at, settled_as, superseded_by \
             FROM self_role_exchanges WHERE event_id = 'journal-defaults'",
        )
        .fetch_one(&pool)
        .await?;
        let attempt: i16 = row.try_get("attempt_no")?;
        assert_eq!(attempt, 1, "one ticket means one wire send");
        let settled_at: Option<String> = row.try_get("settled_at")?;
        assert!(settled_at.is_none(), "new rows start unsettled");
        let settled_as: Option<String> = row.try_get("settled_as")?;
        assert!(settled_as.is_none());
        let superseded_by: Option<String> = row.try_get("superseded_by")?;
        assert!(superseded_by.is_none());
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn ack_existing_pending_to_terminal_transitions_still_work() -> TestResult {
    // Control: the acknowledgement columns must not change the existing
    // pending-to-terminal behavior. Passes before and after the migration.
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        let audit = audit_row("control-event", "guild-1", "member-1");
        let claim = store
            .claim_audit(&audit)
            .await?
            .expect("fresh audit must claim");
        let ticket = store
            .journal_role_exchange(&claim, None, &intent_for("101"))
            .await?
            .expect("journal must succeed");
        assert!(
            store
                .complete_role_exchange(&ticket, ExchangeReceipt::Response { status: 204 })
                .await?,
            "definite completion must still apply"
        );
        let (disposition, status): (String, Option<i16>) = sqlx::query_as(
            "SELECT disposition, response_status FROM self_role_exchanges \
             WHERE event_id = 'control-event'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(disposition, "response");
        assert_eq!(status, Some(204));
        Ok(())
    })
    .await
}
