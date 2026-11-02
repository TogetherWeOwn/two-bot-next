//! Protected acceptance tests: the production gateway batch writer journals
//! its `events` appends into `rollback_journal`.
//!
//! Contract under test (from the card spec, blind to the implementation):
//!
//! - Every `events` row a gateway batch commit inserts leaves exactly one
//!   journal entry (`table` events, row identity, Insert op) inside the
//!   primary dispatch transaction, so N committed rows yield N journal
//!   entries and the row and its evidence commit atomically (no crash
//!   window between a primary commit and a second journal transaction).
//! - Journal failure is evidence-only: the primary batch write (event rows,
//!   members projection, checkpoint) still commits, the gateway's WARN
//!   `rollback_journal record failed` diagnostic is emitted, and the next batch
//!   on a healed schema journals normally (the writer is not latched off).
//! - Duplicates journal nothing: replayed events that lose
//!   `ON CONFLICT DO NOTHING` add no journal rows, and a duplicate dispatch
//!   (same session + sequence) commits nothing at all.
//!
//! Every scenario drives the public seam end to end —
//! `GatewaySessionStore::commit_dispatch` over a hand-built `FunnelBatch` —
//! so the tests pin behavior, not the builder's internal representation of
//! the journal call. The journal tables come from the real store migration
//! chain (`two_bot_store::migrate`, additive-safe over the cutover-migrated
//! disposable database, as in `gateway_tests::persistent`), because the
//! cutover chain does not create them; production runs both chains.
//! Privacy gating and snapshot behavior are unchanged by this slice and stay
//! covered by the existing gateway-session suites.
//!
//! PROTECTED: the paired builder implements production code against this
//! file unchanged. Do not edit these tests to make an implementation pass;
//! if a test is wrong, that is a finding for the test author, not a builder
//! edit.

use two_bot_core::database_tls::TlsPolicy;
use two_bot_core::gateway_funnel::FunnelBatch;
use two_bot_core::gateway_session::{DispatchAction, GatewaySession};
use two_bot_core::{EventType, FunnelEvent};
use two_bot_cutover::db::CutoverDb;
use two_bot_cutover::gateway_session::GatewaySessionStore;
use two_bot_store::journal::{entries_since, restorable_point, watermark_for, JournalOp};
use two_bot_testsupport::TestDatabase;

#[path = "../../core/tests/support/tracing_capture.rs"]
mod tracing_capture;

const GUILD: &str = "1773000000000000001";
const MEMBER: u64 = 123456789012345678;
const OTHER_MEMBER: u64 = 123456789012345679;

const T1: &str = "2026-10-20T12:00:00.000Z";
const T2: &str = "2026-10-20T12:05:00.000Z";
const T3: &str = "2026-10-20T12:10:00.000Z";
const T4: &str = "2026-10-20T12:15:00.000Z";

fn guild_u64() -> u64 {
    GUILD.parse().unwrap()
}

async fn database() -> Option<(TestDatabase, CutoverDb)> {
    let base_url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the gateway batch journal acceptance database"
            );
            eprintln!("SKIP gateway batch journal acceptance: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    let test_db = TestDatabase::create(&base_url, &sqlx::migrate!("./migrations"))
        .await
        .expect("disposable migrated database");
    let fixture_url = match base_url.rsplit_once('/') {
        Some((head, _)) => format!("{head}/{}", test_db.name()),
        None => base_url,
    };
    let db = two_bot_cutover::connect_with_tls(&fixture_url, 5, true, TlsPolicy::LocalOnly)
        .await
        .expect("connect to owned disposable database");
    two_bot_store::migrate(db.pool())
        .await
        .expect("store migrations add the journal tables");
    Some((test_db, db))
}

/// A repeatable join: member plus timestamp tells appends apart, so each
/// distinct `at` is one real insert, never an idempotent duplicate.
fn join(member: u64, at: &str) -> FunnelEvent {
    FunnelEvent {
        guild_id: guild_u64(),
        member_id: Some(member),
        event_type: EventType::MemberJoin,
        occurred_at: at.to_owned(),
        source: "unknown".to_owned(),
        metadata: None,
        dedupe_token: None,
    }
}

/// One serial dispatch: commit one batch per sequence, exactly like the
/// gateway worker does after handling a dispatch.
struct Dispatcher {
    store: GatewaySessionStore,
    sequence: u64,
}

impl Dispatcher {
    fn new(db: &CutoverDb) -> Self {
        Self {
            store: GatewaySessionStore::new(db.pool().clone(), GUILD.to_owned(), 0),
            sequence: 0,
        }
    }

    fn session(&self) -> GatewaySession {
        GatewaySession {
            session_id: "batch-journal-acceptance".into(),
            sequence: self.sequence,
            resume_url: "ws://batch-journal-acceptance".into(),
            updated_at_ms: 1_760_000_000_000 + self.sequence as i64,
        }
    }

    async fn commit(&mut self, batch: FunnelBatch) -> DispatchAction {
        self.sequence += 1;
        let session = self.session();
        self.store
            .commit_dispatch(&session, batch)
            .await
            .expect("gateway batch commit")
    }

    /// Same session + sequence as the last commit: the checkpoint dedupes it.
    async fn replay(&self, batch: FunnelBatch) -> DispatchAction {
        let session = self.session();
        self.store
            .commit_dispatch(&session, batch)
            .await
            .expect("gateway batch replay")
    }
}

async fn journal_count(db: &CutoverDb) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM rollback_journal")
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn event_count(db: &CutoverDb) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn event_ids(db: &CutoverDb) -> Vec<i64> {
    sqlx::query_scalar("SELECT id FROM events ORDER BY id")
        .fetch_all(db.pool())
        .await
        .unwrap()
}

/// N committed batch rows yield N journal entries naming the inserted rows.
#[tokio::test]
async fn batch_commit_journals_every_inserted_row() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let mut dispatcher = Dispatcher::new(&db);

    // Nothing is journaled before the batch writer runs.
    assert_eq!(journal_count(&db).await, 0);
    assert_eq!(watermark_for(db.pool(), "events").await.unwrap(), None);

    let batch = FunnelBatch {
        events: vec![join(MEMBER, T1), join(MEMBER, T2), join(OTHER_MEMBER, T3)],
        ..Default::default()
    };
    assert_eq!(
        dispatcher.commit(batch).await,
        DispatchAction::Apply,
        "the batch commit applies exactly once"
    );

    let event_ids = event_ids(&db).await;
    assert_eq!(event_ids.len(), 3, "three batch events landed");
    assert_eq!(
        journal_count(&db).await,
        3,
        "each committed row left exactly one journal entry"
    );

    // The journal entries are complete insert entries naming the events rows
    // they evidence, in batch order, with no pre-image to restore from.
    let rows = entries_since(db.pool(), "events", 0).await.unwrap();
    assert_eq!(rows.len(), 3);
    for (row, event_id) in rows.iter().zip(&event_ids) {
        assert_eq!(row.table_name, "events");
        assert_eq!(row.op, JournalOp::Insert);
        assert_eq!(
            row.row_identity,
            event_id.to_string(),
            "journal entry names its events row by primary key"
        );
        assert!(row.pre_image.is_none(), "inserts carry no pre-image");
    }
    assert!(rows.windows(2).all(|pair| pair[0].id < pair[1].id));

    // The events watermark and the global restorable point track the newest
    // journaled append.
    let newest = rows.last().unwrap().id;
    assert_eq!(
        watermark_for(db.pool(), "events").await.unwrap(),
        Some(newest)
    );
    assert_eq!(restorable_point(db.pool()).await.unwrap(), newest);

    test_db.close().await.unwrap();
}

fn is_journal_warning(line: &str) -> bool {
    line.starts_with("WARN two_bot_cutover::gateway_session ")
        && line.contains(" message=rollback_journal record failed")
}

#[test]
fn diagnostic_oracle_rejects_lower_levels_and_sqlx_noise() {
    let warning = "WARN two_bot_cutover::gateway_session message=rollback_journal record failed error=missing relation";
    assert!(is_journal_warning(warning));
    for level in ["DEBUG", "INFO"] {
        let downgraded = warning.replacen("WARN", level, 1);
        assert!(
            !is_journal_warning(&downgraded),
            "a {level} message with an error field is not a WARN diagnostic"
        );
    }
    for noise in [
        "DEBUG sqlx::query message=ROLLBACK TO SAVEPOINT rollback_journal error=missing relation",
        "WARN sqlx::query message=rollback_journal record failed error=missing relation",
        "WARN two_bot_cutover::gateway_session message=unrelated warning error=missing relation",
        "WARN two_bot_cutover::gateway_session_extra message=rollback_journal record failed",
    ] {
        assert!(!is_journal_warning(noise), "unrelated diagnostic: {noise}");
    }
    let split = "WARN two_bot_cutover::gateway_session message=unrelated warning\nDEBUG sqlx::query message=rollback_journal record failed error=missing relation";
    assert!(
        !split.lines().any(is_journal_warning),
        "the level, target and message must belong to the same log line"
    );
}

/// Journal-error injection: the primary batch write survives, a diagnostic
/// names the failure, and the healed writer journals again.
#[tokio::test]
async fn journal_write_failure_keeps_primary_batch_write_with_diagnostic() {
    let capture = tracing_capture::global();
    let Some((test_db, db)) = database().await else {
        return;
    };
    let mut dispatcher = Dispatcher::new(&db);

    // Fault injection, local to this fixture's disposable database: the
    // journal table is renamed away, so every journal attempt fails while
    // the primary tables stay fully writable.
    sqlx::query("ALTER TABLE rollback_journal RENAME TO rollback_journal_fault")
        .execute(db.pool())
        .await
        .unwrap();
    let log_start = capture.text().len();

    // The primary batch still commits end to end: outcome, event rows,
    // members projection and checkpoint are exactly as if journaling had
    // worked.
    let batch = FunnelBatch {
        events: vec![join(MEMBER, T1), join(MEMBER, T2)],
        ..Default::default()
    };
    assert_eq!(
        dispatcher.commit(batch).await,
        DispatchAction::Apply,
        "journal failure must not fail the batch"
    );
    assert_eq!(event_count(&db).await, 2);
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM members WHERE guild_id = $1")
        .bind(GUILD)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        members, 1,
        "the members projection is part of the primary write"
    );
    let checkpoint: i64 =
        sqlx::query_scalar("SELECT seq FROM gateway_sessions WHERE guild_id = $1 AND shard_id = 0")
            .bind(GUILD)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(checkpoint, 1, "the checkpoint is part of the primary write");

    // Require the level, target and message on one line: SQLx DEBUG query
    // text and an `error=` field are not a gateway WARN diagnostic.
    let logged = capture.text()[log_start..].to_owned();
    assert!(
        logged.lines().any(is_journal_warning),
        "faulted batch must emit the gateway rollback-journal WARN diagnostic, got: {logged}"
    );

    // No partial evidence survived the fault: no journal row, no watermark.
    sqlx::query("ALTER TABLE rollback_journal_fault RENAME TO rollback_journal")
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(journal_count(&db).await, 0);
    assert_eq!(watermark_for(db.pool(), "events").await.unwrap(), None);

    // The writer is not latched off by the failure: the next batch on the
    // healed schema journals normally.
    let healed = FunnelBatch {
        events: vec![join(MEMBER, T3)],
        ..Default::default()
    };
    let healed_log_start = capture.text().len();
    assert_eq!(
        dispatcher.commit(healed).await,
        DispatchAction::Apply,
        "healed batch applies"
    );
    assert_eq!(
        journal_count(&db).await,
        1,
        "writer recovers after the fault"
    );
    assert_eq!(event_count(&db).await, 3);
    let healed_logged = capture.text()[healed_log_start..].to_owned();
    assert!(
        !healed_logged.lines().any(is_journal_warning),
        "healed batch must not emit the journal failure WARN diagnostic, got: {healed_logged}"
    );

    test_db.close().await.unwrap();
}

/// Duplicate replays journal nothing: conflict losers add no rows and no
/// entries, and a duplicate dispatch commits nothing at all.
#[tokio::test]
async fn duplicate_replays_journal_nothing() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let mut dispatcher = Dispatcher::new(&db);

    // The first batch commits two rows.
    let first = FunnelBatch {
        events: vec![join(MEMBER, T1), join(OTHER_MEMBER, T2)],
        ..Default::default()
    };
    assert_eq!(
        dispatcher.commit(first).await,
        DispatchAction::Apply,
        "first batch applies"
    );
    assert_eq!(event_count(&db).await, 2);

    // Replaying the same events in a new dispatch inserts nothing: the same
    // idempotency keys lose `ON CONFLICT DO NOTHING`.
    let replayed = FunnelBatch {
        events: vec![join(MEMBER, T1), join(OTHER_MEMBER, T2)],
        ..Default::default()
    };
    assert_eq!(
        dispatcher.commit(replayed).await,
        DispatchAction::Apply,
        "replayed batch still applies its checkpoint"
    );
    assert_eq!(event_count(&db).await, 2, "replayed events insert no rows");

    // ... and journals nothing new: the journal still covers exactly the
    // committed rows.
    assert_eq!(
        journal_count(&db).await,
        event_count(&db).await,
        "journal covers exactly the committed rows; duplicates journal nothing"
    );

    // A duplicate dispatch (same session + sequence) commits nothing at all.
    let journal_before = journal_count(&db).await;
    let duplicate = FunnelBatch {
        events: vec![join(MEMBER, T4)],
        ..Default::default()
    };
    assert_eq!(
        dispatcher.replay(duplicate).await,
        DispatchAction::Duplicate,
        "same session + sequence is a duplicate dispatch"
    );
    assert_eq!(
        event_count(&db).await,
        2,
        "duplicate dispatch inserts no rows"
    );
    assert_eq!(
        journal_count(&db).await,
        journal_before,
        "duplicate dispatch journals nothing"
    );

    test_db.close().await.unwrap();
}
