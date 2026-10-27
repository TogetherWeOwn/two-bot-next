//! Acceptance tests: settle self-role sends through the acknowledgement columns.
//!
//! Protected test file: the build card implements against these tests and
//! must not edit them. If a pinned name below cannot be implemented as
//! written, stop and escalate instead of changing the tests.
//!
//! Context: migration `0449_self_role_ticket_acknowledgement.sql` added
//! `settled_at/settled_as/superseded_by/attempt_no/outcome_class` to
//! `self_role_exchanges`, but no runtime writer exists yet: ambiguous sends
//! stay invisible. This file pins the settlement lifecycle through the real
//! store APIs plus direct SQL reads. The sibling acknowledgement-columns file
//! (`self_role_ack_acceptance.rs`) owns the migration, caps and sizing; this
//! file owns the writers and the recovery contract.
//!
//! Contract pinned here (build card implements exactly this):
//!
//! - Definite versus ambiguous is the store's existing definition
//!   (`merge_receipt_evidence`): `NoSend`, `Response 204` and `Response
//!   400..=499` are definite; any other received `200..=599` status is
//!   ambiguous. `SelfRoleStore::classify_ambiguous_response(status: u16) ->
//!   Option<&'static str>` is a pure associated function returning
//!   `Some("response_ambiguous")` for ambiguous statuses and `None` for
//!   definite ones and for values outside `200..=599` (which
//!   `complete_role_exchange` rejects before classification).
//! - `complete_role_exchange` (existing API, unchanged signature) writes
//!   `outcome_class = 'response_ambiguous'` when completing with an ambiguous
//!   response status; definite completions keep `outcome_class` NULL. Exact
//!   replay stays idempotent and keeps the class.
//! - `note_ambiguous_outcome(&self, ticket, outcome_class: &str) ->
//!   Result<bool, StoreError>` records a timeout or loop-exhaustion outcome on
//!   a still-pending ticket without completing it (disposition stays
//!   `pending`, `completed_at` stays NULL): `timeout_no_response` for a send
//!   with no received response, `loop_exhaustion` for a bounded retry loop
//!   that gave up. Any other class is `Err`. Returns true when it wrote.
//! - `note_wire_retry(&self, ticket) -> Result<bool, StoreError>` bumps
//!   `attempt_no` by one on a pending ticket. Returns true when it bumped.
//!   Fresh tickets start at 1.
//! - `supersede_role_exchange(&self, superseded, replacement) ->
//!   Result<bool, StoreError>` settles the superseded row: `superseded_by`
//!   names the replacement ticket's `exchange_id`, `settled_as` becomes
//!   `'superseded_settled'`, `settled_at` is set. The replacement row is
//!   untouched. Returns true when it wrote.
//! - `retire_role_receipts` (existing API, unchanged signature and unchanged
//!   evidence semantics) additionally writes `settled_at`/`settled_as =
//!   'expired_unknown'` on each row it retires that carries an ambiguous
//!   `outcome_class` (completed but never resolved). Retired definite rows
//!   keep `settled_*` NULL.
//! - `expire_stale_exchanges(&self, older_than: time::Duration) ->
//!   Result<u64, StoreError>` settles pending, unsettled rows with `created_at`
//!   older than the bound (database clock) as `'expired_unknown'` with
//!   `settled_at` set, and returns how many rows it settled. Fresh pending
//!   rows are untouched.
//! - `pending_retry_candidates(pool: &PgPool, event_id: &str) ->
//!   Result<Vec<String>, StoreError>` is an associated function listing the
//!   `exchange_id`s of pending, unsettled rows for the event (ordered): the
//!   recovery retry set. Settled rows are skipped; genuinely pending rows are
//!   returned.
//! - Runtime wiring (bot crate, builder's own tests, not this file): a
//!   journaled send with no received response records `timeout_no_response`;
//!   a bounded retry loop that gives up records `loop_exhaustion`; wire
//!   retries bump `attempt_no`; replacement sends supersede the old ticket.
//!
//! Test shapes: offline unit tests (no database, run by default) pin the pure
//! classifier through a baseline adapter trait; the builder's inherent items
//! shadow the trait (inherent methods take precedence in method resolution).
//! Database tests (`#[ignore]`, disposable agent-testdb schema only) pin the
//! writers through the real store APIs plus direct SQL reads. No live
//! Discord, no staging or production database.
//!
//! CI routing note for the builder: this file contains `#[ignore]` targets,
//! so the test-inventory gate requires an explicit `--test
//! self_role_settlement_acceptance` step (the self-role-store job is the
//! natural home). Add it in the implementation PR alongside this file.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use two_bot_core::{PanelMode, RoleOperation, SettledOutcome};
use two_bot_cutover::self_role_store::{
    AuditEffects, ExchangeIntent, ExchangeReceipt, ExchangeTicket, SelfRoleAudit, SelfRoleStore,
    StoreError,
};

// ---------------------------------------------------------------------------
// Baseline adapter (offline behavior + unfiltered recovery query)
// ---------------------------------------------------------------------------

/// Baseline shape of the settlement surface. At baseline nothing writes the
/// acknowledgement columns, so the writer baselines report "wrote nothing"
/// and the classifier reports "definite"; every behavioral test below fails
/// until the builder adds the inherent items, which shadow these calls. The
/// recovery-listing baseline runs the real pending query WITHOUT the settled
/// filter, so the skip-settled test fails while the include-pending test
/// passes: together they prove the filter is the missing behavior.
trait BaselineSettlement {
    fn classify_ambiguous_response(status: u16) -> Option<&'static str>;
    async fn note_wire_retry(&self, ticket: &ExchangeTicket) -> Result<bool, StoreError>;
    async fn note_ambiguous_outcome(
        &self,
        ticket: &ExchangeTicket,
        outcome_class: &str,
    ) -> Result<bool, StoreError>;
    async fn supersede_role_exchange(
        &self,
        superseded: &ExchangeTicket,
        replacement: &ExchangeTicket,
    ) -> Result<bool, StoreError>;
    async fn expire_stale_exchanges(&self, older_than: time::Duration) -> Result<u64, StoreError>;
    async fn pending_retry_candidates(
        pool: &PgPool,
        event_id: &str,
    ) -> Result<Vec<String>, StoreError>;
}

impl BaselineSettlement for SelfRoleStore {
    fn classify_ambiguous_response(_status: u16) -> Option<&'static str> {
        None
    }

    async fn note_wire_retry(&self, _ticket: &ExchangeTicket) -> Result<bool, StoreError> {
        Ok(false)
    }

    async fn note_ambiguous_outcome(
        &self,
        _ticket: &ExchangeTicket,
        _outcome_class: &str,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }

    async fn supersede_role_exchange(
        &self,
        _superseded: &ExchangeTicket,
        _replacement: &ExchangeTicket,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }

    async fn expire_stale_exchanges(&self, _older_than: time::Duration) -> Result<u64, StoreError> {
        Ok(0)
    }

    async fn pending_retry_candidates(
        pool: &PgPool,
        event_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        // Baseline: the real pending query WITHOUT the settled filter. The
        // builder's inherent function adds `AND settled_at IS NULL`.
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT exchange_id FROM self_role_exchanges \
             WHERE event_id = $1 AND disposition = 'pending' ORDER BY exchange_id",
        )
        .bind(event_id)
        .fetch_all(pool)
        .await?;
        Ok(rows.into_iter().map(|row| row.0).collect())
    }
}

// ---------------------------------------------------------------------------
// Offline tests: the definite/ambiguous classifier (no database)
// ---------------------------------------------------------------------------

#[test]
fn settlement_classifier_marks_ambiguous_statuses() {
    for status in [200, 201, 206, 301, 302, 500, 503, 599] {
        assert_eq!(
            SelfRoleStore::classify_ambiguous_response(status),
            Some("response_ambiguous"),
            "received-but-not-definite status {status} must classify ambiguous"
        );
    }
}

#[test]
fn settlement_classifier_keeps_definite_and_rejected_statuses_null() {
    // 204, the 4xx band (429 included: rate-limited is a definite refusal
    // per merge_receipt_evidence), and values complete_role_exchange rejects.
    for status in [199, 204, 400, 403, 404, 429, 499, 600] {
        assert_eq!(
            SelfRoleStore::classify_ambiguous_response(status),
            None,
            "status {status} must stay definite (NULL outcome)"
        );
    }
}

#[test]
fn settlement_baseline_classifier_stays_live() {
    // Explicit trait path: keeps BaselineSettlement::classify_ambiguous_response
    // used after the builder's inherent function shadows the unqualified calls
    // above, so clippy `-D warnings` stays green in both states. The async
    // baseline items are kept live by explicit-path calls in
    // settlement_baseline_items_stay_live below.
    let baseline: Option<&str> =
        <SelfRoleStore as BaselineSettlement>::classify_ambiguous_response(500);
    assert_eq!(baseline, None, "baseline adapter reports definite");
}

// ---------------------------------------------------------------------------
// Disposable-database fixture (agent-testdb only, isolated schema per test)
// ---------------------------------------------------------------------------

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const BASE_MIGRATIONS: [&str; 8] = [
    include_str!("../migrations/0200_self_roles.sql"),
    include_str!("../migrations/0201_self_role_intent_initialization.sql"),
    include_str!("../migrations/0202_self_role_compensation_phase.sql"),
    include_str!("../migrations/0203_self_role_pending_exchange.sql"),
    include_str!("../migrations/0204_self_role_terminal_repair.sql"),
    include_str!("../migrations/0205_self_role_exchange_receipts.sql"),
    include_str!("../migrations/0206_self_role_exchange_baselines.sql"),
    include_str!("../migrations/0449_self_role_ticket_acknowledgement.sql"),
];

static SCHEMA_SEQ: AtomicU64 = AtomicU64::new(0);

fn schema_name_at(pid: u32, timestamp: std::time::Duration) -> String {
    // Low 64 bits of the nanosecond clock: real timestamps fit entirely, and
    // Duration::MAX stays bounded. The atomic sequence distinguishes identical
    // PID/clock samples across concurrent fixtures in this process.
    let nonce = timestamp.as_nanos() as u64;
    let sequence = SCHEMA_SEQ.fetch_add(1, Ordering::Relaxed);
    // At most 59 bytes: prefix 17 + PID 8 + separators 2 + nanos 16 + sequence 16.
    format!("self_role_settle_{pid:x}_{nonce:x}_{sequence:016x}")
}

#[test]
fn settlement_schema_names_are_distinct_and_bounded_when_the_clock_repeats() {
    let barrier = std::sync::Barrier::new(8);
    let names = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    (0..32)
                        .map(|_| schema_name_at(u32::MAX, std::time::Duration::MAX))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("schema allocation worker"))
            .collect::<Vec<_>>()
    });
    assert_eq!(names.len(), 256);
    assert_eq!(
        names.iter().collect::<std::collections::HashSet<_>>().len(),
        names.len(),
        "concurrent fixtures must not share a schema when PID and timestamp repeat"
    );
    for name in names {
        assert!(name.len() <= 63, "Postgres must not truncate {name}");
        assert!(name.starts_with("self_role_settle_"));
        assert!(name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'));
    }
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
    let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    let schema = schema_name_at(std::process::id(), timestamp);
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

async fn journal_ticket(
    store: &SelfRoleStore,
    event: &str,
    role_id: &str,
) -> Result<ExchangeTicket, Box<dyn std::error::Error + Send + Sync>> {
    let audit = audit_row(event, "guild-1", "member-1");
    let claim = store
        .claim_audit(&audit)
        .await?
        .expect("fresh audit must claim");
    let ticket = store
        .journal_role_exchange(&claim, None, &intent_for(role_id))
        .await?
        .expect("journal must succeed");
    Ok(ticket)
}

async fn exchange_row(
    pool: &PgPool,
    event: &str,
) -> Result<
    (
        String,
        Option<i16>,
        Option<String>,
        i16,
        Option<String>,
        Option<String>,
    ),
    Box<dyn std::error::Error + Send + Sync>,
> {
    // (disposition, response_status, outcome_class, attempt_no, settled_as,
    // settled_at-is-set)
    let row: (
        String,
        Option<i16>,
        Option<String>,
        i16,
        Option<String>,
        bool,
    ) = sqlx::query_as(
        "SELECT disposition, response_status, outcome_class, attempt_no, \
         settled_as, settled_at IS NOT NULL \
         FROM self_role_exchanges WHERE event_id = $1",
    )
    .bind(event)
    .fetch_one(pool)
    .await?;
    Ok((
        row.0,
        row.1,
        row.2,
        row.3,
        row.4,
        row.5.then_some("set".to_owned()),
    ))
}

// ---------------------------------------------------------------------------
// Database tests: ambiguous completion writes outcome_class; definite stays NULL
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_ambiguous_completion_writes_outcome_class() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        let ticket = journal_ticket(&store, "ambiguous-complete", "101").await?;
        assert!(
            store
                .complete_role_exchange(&ticket, ExchangeReceipt::Response { status: 500 })
                .await?,
            "ambiguous completion must still apply"
        );
        let (disposition, status, outcome, _, _, _) =
            exchange_row(&pool, "ambiguous-complete").await?;
        assert_eq!(disposition, "response");
        assert_eq!(status, Some(500));
        assert_eq!(
            outcome.as_deref(),
            Some("response_ambiguous"),
            "ambiguous completion must persist its outcome class"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_definite_completions_keep_outcome_null() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        for (n, receipt) in [
            ExchangeReceipt::Response { status: 204 },
            ExchangeReceipt::Response { status: 403 },
            ExchangeReceipt::Response { status: 429 },
            ExchangeReceipt::NoSend,
        ]
        .into_iter()
        .enumerate()
        {
            let event = format!("definite-complete-{n}");
            let ticket = journal_ticket(&store, &event, "101").await?;
            assert!(
                store.complete_role_exchange(&ticket, receipt).await?,
                "definite completion {receipt:?} must still apply"
            );
            let (_, _, outcome, _, _, _) = exchange_row(&pool, &event).await?;
            assert!(
                outcome.is_none(),
                "definite completion {receipt:?} must keep outcome_class NULL, got {outcome:?}"
            );
        }
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_ambiguous_completion_replay_keeps_class() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        let ticket = journal_ticket(&store, "ambiguous-replay", "101").await?;
        let receipt = ExchangeReceipt::Response { status: 503 };
        assert!(store.complete_role_exchange(&ticket, receipt).await?);
        assert!(
            store.complete_role_exchange(&ticket, receipt).await?,
            "exact replay must stay idempotent"
        );
        let (_, _, outcome, _, _, _) = exchange_row(&pool, "ambiguous-replay").await?;
        assert_eq!(
            outcome.as_deref(),
            Some("response_ambiguous"),
            "replay must keep the ambiguous class"
        );
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------
// Database tests: timeout / loop-exhaustion outcomes on pending tickets
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_timeout_outcome_pins_pending_without_receipt() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        let ticket = journal_ticket(&store, "timeout-outcome", "101").await?;
        assert!(
            store
                .note_ambiguous_outcome(&ticket, "timeout_no_response")
                .await?,
            "timeout outcome must record on a pending ticket"
        );
        let row: (String, Option<String>, bool) = sqlx::query_as(
            "SELECT disposition, outcome_class, completed_at IS NULL \
             FROM self_role_exchanges WHERE event_id = 'timeout-outcome'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(row.0, "pending", "a timeout is not a receipt");
        assert_eq!(row.1.as_deref(), Some("timeout_no_response"));
        assert!(row.2, "a timeout must not fabricate completion");
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_loop_exhaustion_outcome_round_trips() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        let ticket = journal_ticket(&store, "loop-outcome", "101").await?;
        assert!(
            store
                .note_ambiguous_outcome(&ticket, "loop_exhaustion")
                .await?,
            "loop-exhaustion outcome must record on a pending ticket"
        );
        let (stored,): (Option<String>,) = sqlx::query_as(
            "SELECT outcome_class FROM self_role_exchanges WHERE event_id = 'loop-outcome'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(stored.as_deref(), Some("loop_exhaustion"));
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_unknown_outcome_class_is_refused() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        let ticket = journal_ticket(&store, "bogus-outcome", "101").await?;
        let result = store.note_ambiguous_outcome(&ticket, "bogus").await;
        assert!(
            result.is_err(),
            "an unknown outcome class must be refused, got {result:?}"
        );
        let (stored,): (Option<String>,) = sqlx::query_as(
            "SELECT outcome_class FROM self_role_exchanges WHERE event_id = 'bogus-outcome'",
        )
        .fetch_one(&pool)
        .await?;
        assert!(stored.is_none(), "a refused outcome must write nothing");
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------
// Database tests: wire retries and replacement tickets
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_wire_retry_bumps_attempt_no() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        let ticket = journal_ticket(&store, "wire-retry", "101").await?;
        let (_, _, _, first, _, _) = exchange_row(&pool, "wire-retry").await?;
        assert_eq!(first, 1, "a fresh ticket represents one send");
        assert!(
            store.note_wire_retry(&ticket).await?,
            "first retry must bump"
        );
        assert!(
            store.note_wire_retry(&ticket).await?,
            "second retry must bump"
        );
        let (_, _, _, attempt, _, _) = exchange_row(&pool, "wire-retry").await?;
        assert_eq!(attempt, 3, "two retries must count three sends");
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_replacement_links_and_settles_superseded_row() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        let audit = audit_row("supersede-event", "guild-1", "member-1");
        let claim = store
            .claim_audit(&audit)
            .await?
            .expect("fresh audit must claim");
        let old = store
            .journal_role_exchange(&claim, None, &intent_for("101"))
            .await?
            .expect("old ticket must journal");
        let replacement = store
            .journal_role_exchange(&claim, None, &intent_for("101"))
            .await?
            .expect("replacement ticket must journal");
        assert!(
            store.supersede_role_exchange(&old, &replacement).await?,
            "supersede must link the replacement"
        );
        // Exactly the old row names its replacement and settles; the link
        // resolves to the other row for the same event.
        let linked: Vec<(String, String, bool)> = sqlx::query_as(
            "SELECT o.superseded_by, o.settled_as, o.settled_at IS NOT NULL \
             FROM self_role_exchanges o \
             WHERE o.event_id = 'supersede-event' AND o.superseded_by IS NOT NULL",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(linked.len(), 1, "exactly the old row must link");
        assert_eq!(linked[0].1, "superseded_settled");
        assert!(linked[0].2, "the old row must carry settled_at");
        let (resolves,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM self_role_exchanges r \
             WHERE r.event_id = 'supersede-event' AND r.exchange_id = $1",
        )
        .bind(&linked[0].0)
        .fetch_one(&pool)
        .await?;
        assert_eq!(resolves, 1, "superseded_by must name the replacement row");
        let (replacement_settled,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM self_role_exchanges \
             WHERE event_id = 'supersede-event' AND settled_at IS NOT NULL",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            replacement_settled, 1,
            "the replacement row stays unsettled"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_fresh_ticket_starts_unsettled_single_attempt() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        journal_ticket(&store, "fresh-ticket", "101").await?;
        let (disposition, _, outcome, attempt, settled_as, settled_at) =
            exchange_row(&pool, "fresh-ticket").await?;
        assert_eq!(disposition, "pending");
        assert_eq!(attempt, 1);
        assert!(outcome.is_none());
        assert!(settled_as.is_none());
        assert!(settled_at.is_none());
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------------------
// Database tests: retirement and age-out settle; recovery skips settled rows
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_retire_settles_ambiguous_rows_as_expired_unknown() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        let audit = audit_row("retire-ambiguous", "guild-1", "member-1");
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
                .complete_role_exchange(&ticket, ExchangeReceipt::Response { status: 500 })
                .await?
        );
        let evidence = store
            .retire_role_receipts(&claim, None)
            .await?
            .expect("live owner must retire");
        // Existing evidence semantics are preserved: the ambiguous effect
        // stays unresolved even as the ticket settles.
        assert!(
            evidence
                .effects
                .unresolved_added_role_ids
                .contains(&"101".to_owned()),
            "retirement must keep ambiguous effect uncertainty"
        );
        let row: (bool, Option<String>, bool) = sqlx::query_as(
            "SELECT retired_at IS NOT NULL, settled_as, settled_at IS NOT NULL \
             FROM self_role_exchanges WHERE event_id = 'retire-ambiguous'",
        )
        .fetch_one(&pool)
        .await?;
        assert!(row.0, "the ticket must retire");
        assert_eq!(
            row.1.as_deref(),
            Some("expired_unknown"),
            "retiring an ambiguous ticket must settle it as expired_unknown"
        );
        assert!(row.2, "the settled row must carry settled_at");
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_retire_keeps_definite_rows_unsettled() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        let audit = audit_row("retire-definite", "guild-1", "member-1");
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
                .await?
        );
        store
            .retire_role_receipts(&claim, None)
            .await?
            .expect("live owner must retire");
        let row: (bool, Option<String>, bool) = sqlx::query_as(
            "SELECT retired_at IS NOT NULL, settled_as, settled_at IS NOT NULL \
             FROM self_role_exchanges WHERE event_id = 'retire-definite'",
        )
        .fetch_one(&pool)
        .await?;
        assert!(row.0, "the definite ticket must retire");
        assert!(row.1.is_none(), "definite rows keep settled_as NULL");
        assert!(!row.2, "definite rows keep settled_at NULL");
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_expire_settles_stale_pending_as_expired_unknown() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        journal_ticket(&store, "stale-pending", "101").await?;
        journal_ticket(&store, "fresh-pending", "102").await?;
        sqlx::query(
            "UPDATE self_role_exchanges SET created_at = clock_timestamp() - make_interval(hours => 2) \
             WHERE event_id = 'stale-pending'",
        )
        .execute(&pool)
        .await?;
        let settled = store
            .expire_stale_exchanges(time::Duration::hours(1))
            .await?;
        assert_eq!(settled, 1, "exactly the stale ticket must expire");
        let stale: (String, Option<String>, bool) = sqlx::query_as(
            "SELECT disposition, settled_as, settled_at IS NOT NULL \
             FROM self_role_exchanges WHERE event_id = 'stale-pending'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(stale.0, "pending", "expiry settles; it never invents a receipt");
        assert_eq!(stale.1.as_deref(), Some("expired_unknown"));
        assert!(stale.2);
        let fresh: (Option<String>, bool) = sqlx::query_as(
            "SELECT settled_as, settled_at IS NOT NULL \
             FROM self_role_exchanges WHERE event_id = 'fresh-pending'",
        )
        .fetch_one(&pool)
        .await?;
        assert!(fresh.0.is_none(), "a fresh pending ticket must survive expiry");
        assert!(!fresh.1);
        let again = store
            .expire_stale_exchanges(time::Duration::hours(1))
            .await?;
        assert_eq!(again, 0, "expiry must not re-settle settled rows");
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_retry_candidates_include_genuinely_pending() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        journal_ticket(&store, "candidate-pending", "101").await?;
        let (id,): (String,) = sqlx::query_as(
            "SELECT exchange_id FROM self_role_exchanges WHERE event_id = 'candidate-pending'",
        )
        .fetch_one(&pool)
        .await?;
        let candidates =
            SelfRoleStore::pending_retry_candidates(&pool, "candidate-pending").await?;
        assert!(
            candidates.contains(&id),
            "a genuinely pending ticket must be a retry candidate"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_retry_candidates_skip_settled_rows() -> TestResult {
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        journal_ticket(&store, "candidate-settled", "101").await?;
        // Simulate post-builder settled state directly: the recovery filter
        // reads the settled columns, however they were written.
        sqlx::query(
            "UPDATE self_role_exchanges SET settled_as = 'superseded_settled', \
             settled_at = clock_timestamp(), superseded_by = 'replacement-ticket' \
             WHERE event_id = 'candidate-settled'",
        )
        .execute(&pool)
        .await?;
        let (id,): (String,) = sqlx::query_as(
            "SELECT exchange_id FROM self_role_exchanges WHERE event_id = 'candidate-settled'",
        )
        .fetch_one(&pool)
        .await?;
        let candidates =
            SelfRoleStore::pending_retry_candidates(&pool, "candidate-settled").await?;
        assert!(
            !candidates.contains(&id),
            "recovery must skip settled rows instead of re-probing them"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn settlement_baseline_items_stay_live() -> TestResult {
    // Explicit trait paths: keep every async baseline item used after the
    // builder's inherent items shadow the unqualified calls above, so clippy
    // `-D warnings` stays green in both states.
    isolated(|pool| async move {
        let store = SelfRoleStore::new(pool.clone());
        let audit = audit_row("baseline-live", "guild-1", "member-1");
        let claim = store
            .claim_audit(&audit)
            .await?
            .expect("fresh audit must claim");
        let ticket = store
            .journal_role_exchange(&claim, None, &intent_for("101"))
            .await?
            .expect("journal must succeed");
        let retry: Result<bool, StoreError> =
            <SelfRoleStore as BaselineSettlement>::note_wire_retry(&store, &ticket).await;
        assert!(!retry.unwrap());
        let noted: Result<bool, StoreError> =
            <SelfRoleStore as BaselineSettlement>::note_ambiguous_outcome(
                &store,
                &ticket,
                "timeout_no_response",
            )
            .await;
        assert!(!noted.unwrap());
        let superseded: Result<bool, StoreError> =
            <SelfRoleStore as BaselineSettlement>::supersede_role_exchange(
                &store, &ticket, &ticket,
            )
            .await;
        assert!(!superseded.unwrap());
        let expired: Result<u64, StoreError> =
            <SelfRoleStore as BaselineSettlement>::expire_stale_exchanges(
                &store,
                time::Duration::hours(1),
            )
            .await;
        assert_eq!(expired.unwrap(), 0);
        // The baseline listing is deliberately unfiltered: it still returns
        // the pending ticket here, documenting what the builder changes.
        let candidates: Result<Vec<String>, StoreError> =
            <SelfRoleStore as BaselineSettlement>::pending_retry_candidates(&pool, "baseline-live")
                .await;
        assert_eq!(candidates.unwrap().len(), 1);
        Ok(())
    })
    .await
}
