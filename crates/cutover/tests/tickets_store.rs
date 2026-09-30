//! Opt-in integration proof: agent-testdb locally, CI service container in Actions.
//! No DATABASE_URL or staging/production credentials are read by this test.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use std::sync::atomic::{AtomicU64, Ordering};
use two_bot_core::tickets::{
    format_transcript, recovery_action, OpenDecision, RecoveryAction, Ticket, TicketError,
    TicketStatus, TranscriptMessage, COOLDOWN_SECONDS, INTERRUPTED_AFTER_MS,
};
use two_bot_cutover::tickets::{OpenResult, StoreError, TicketStore};

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    pool: PgPool,
    admin: PgPool,
    schema: String,
}

impl TestDb {
    async fn new(legacy: bool) -> Self {
        let host = match std::env::var("TICKET_TEST_DB_HOST").as_deref() {
            Ok("127.0.0.1") if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") => {
                "127.0.0.1"
            }
            Ok("agent-testdb") | Err(_) => "agent-testdb",
            _ => panic!(
                "ticket DB tests permit only agent-testdb or the GitHub Actions service container"
            ),
        };
        let options = PgConnectOptions::new()
            .host(host)
            .port(5432)
            .username("agent_test")
            .password("")
            .database("agent_test");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .expect("connect to test container (do not substitute credentials)");
        let schema = format!(
            "tickets_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        sqlx::QueryBuilder::new("CREATE SCHEMA ")
            .push(&schema) // Trusted identifier: only a fixed prefix and numeric pid/counter.
            .build()
            .execute(&admin)
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options.options([("search_path", schema.clone())]))
            .await
            .unwrap();
        if legacy {
            sqlx::raw_sql("CREATE TABLE tickets (id TEXT PRIMARY KEY, guild_id TEXT NOT NULL, channel_id TEXT NOT NULL UNIQUE, opener_id TEXT NOT NULL, claimed_by TEXT, status TEXT NOT NULL CHECK (status IN ('open', 'closed')), created_at TEXT NOT NULL, closed_at TEXT); CREATE UNIQUE INDEX idx_tickets_one_open ON tickets (guild_id, opener_id) WHERE status = 'open'; CREATE TABLE ticket_transcripts (ticket_id TEXT PRIMARY KEY REFERENCES tickets(id) ON DELETE CASCADE, guild_id TEXT NOT NULL, channel_id TEXT NOT NULL, opener_id TEXT NOT NULL, claimed_by TEXT, content TEXT NOT NULL, message_count INTEGER NOT NULL, created_at TEXT NOT NULL);")
                .execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO tickets (id, guild_id, channel_id, opener_id, status, created_at) VALUES ('old', 'guild', 'old-channel', 'old-opener', 'closed', '2026-09-08T12:00:00.000Z')")
                .execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO ticket_transcripts (ticket_id, guild_id, channel_id, opener_id, content, message_count, created_at) VALUES ('old', 'guild', 'old-channel', 'old-opener', 'existing transcript', 1, '2026-09-08T12:00:00.000Z')")
                .execute(&pool).await.unwrap();
        }
        sqlx::raw_sql(include_str!("../migrations/0210_tickets.sql"))
            .execute(&pool)
            .await
            .unwrap();
        Self {
            pool,
            admin,
            schema,
        }
    }

    fn store(&self, guild: &str) -> TicketStore {
        TicketStore::new(self.pool.clone(), guild.to_owned()).unwrap()
    }

    async fn close(self) {
        self.pool.close().await;
        // This schema was created above with a process-local name in testdb.
        sqlx::QueryBuilder::new("DROP SCHEMA ")
            .push(&self.schema)
            .push(" CASCADE")
            .build()
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}

fn created(result: OpenResult) -> Ticket {
    match result {
        OpenResult::Created(ticket) => ticket,
        other => panic!("expected reservation, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires agent-testdb or a CI service container"]
async fn concurrent_reservations_claims_and_closes_have_one_winner() {
    let db = TestDb::new(false).await;
    let store = db.store("guild");
    let (a, b) = tokio::join!(
        store.reserve("a", "member", 0, COOLDOWN_SECONDS),
        store.reserve("b", "member", 0, COOLDOWN_SECONDS)
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert!(matches!(
        (&a, &b),
        (
            OpenResult::Created(_),
            OpenResult::Refused(OpenDecision::Existing { .. })
        ) | (
            OpenResult::Refused(OpenDecision::Existing { .. }),
            OpenResult::Created(_)
        )
    ));
    let ticket = match a {
        OpenResult::Created(ticket) => ticket,
        _ => created(b),
    };
    store.record_channel(&ticket.id, "channel").await.unwrap();
    store.activate(&ticket.id, "channel").await.unwrap();
    let (a, b) = tokio::join!(
        store.claim(&ticket.id, "staff-a"),
        store.claim(&ticket.id, "staff-b")
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let (a, b) = tokio::join!(
        store.begin_close(&ticket.id, 10),
        store.begin_close(&ticket.id, 20)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let close = a.or(b).unwrap();
    store
        .save_transcript(
            &ticket.id,
            close.closing_started_at.unwrap(),
            30,
            format_transcript(vec![]),
        )
        .await
        .unwrap();
    store.finish_cleanup(&ticket.id, 40).await.unwrap();
    assert_eq!(
        store
            .reserve("cooldown", "member", 299_999, COOLDOWN_SECONDS)
            .await
            .unwrap(),
        OpenResult::Refused(OpenDecision::Cooldown)
    );
    assert!(matches!(
        store
            .reserve("boundary", "member", 300_000, COOLDOWN_SECONDS)
            .await
            .unwrap(),
        OpenResult::Created(_)
    ));
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or a CI service container"]
async fn restart_recovers_open_tickets_and_fences_stale_close_snapshot() {
    let db = TestDb::new(false).await;
    let store = db.store("guild");
    let ticket = created(
        store
            .reserve("ticket", "member", 0, COOLDOWN_SECONDS)
            .await
            .unwrap(),
    );
    store.activate(&ticket.id, "channel").await.unwrap();
    // A new handle has no in-memory tickets; it reattaches from persisted rows.
    let restarted = db.store("guild");
    let loaded = restarted.recoverable().await.unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(
        recovery_action(&loaded[0], 1, false),
        RecoveryAction::ReattachOpenControls {
            channel_id: "channel".into()
        }
    );
    store.begin_close(&ticket.id, 10).await.unwrap();
    restarted.reopen_interrupted(&ticket.id, 10).await.unwrap();
    restarted.begin_close(&ticket.id, 20).await.unwrap();
    let stale = store
        .save_transcript(&ticket.id, 10, 30, format_transcript(vec![]))
        .await;
    assert!(matches!(
        stale,
        Err(StoreError::Domain(TicketError::StaleClose))
    ));
    assert!(!store.transcript_exists(&ticket.id).await.unwrap());
    let snapshot = format_transcript(vec![TranscriptMessage {
        created_at: 0,
        author_tag: "member".into(),
        content: "complete snapshot".into(),
        attachment_urls: vec![],
    }]);
    let saved = restarted
        .save_transcript(&ticket.id, 20, 30, snapshot)
        .await
        .unwrap();
    assert_eq!(saved.message_count, 1);
    assert!(saved.content.contains("complete snapshot"));
    let persisted = store.get(&ticket.id).await.unwrap().unwrap();
    assert_eq!(persisted.status, TicketStatus::CleanupPending);
    assert_eq!(
        recovery_action(&persisted, INTERRUPTED_AFTER_MS, true),
        RecoveryAction::RetryCleanup {
            channel_id: "channel".into()
        }
    );
    assert!(store.reopen_interrupted(&ticket.id, 20).await.is_err());
    assert!(store
        .save_transcript(&ticket.id, 20, 40, format_transcript(vec![]))
        .await
        .is_err());
    let (content,): (String,) =
        sqlx::query_as("SELECT content FROM ticket_transcripts WHERE ticket_id = $1")
            .bind(&ticket.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(content, saved.content);
    store.finish_cleanup(&ticket.id, 40).await.unwrap();
    assert!(restarted.recoverable().await.unwrap().is_empty());
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or a CI service container"]
async fn transcript_insert_failure_rolls_back_state_and_body() {
    let db = TestDb::new(false).await;
    let store = db.store("guild");
    store
        .reserve("ticket", "member", 0, COOLDOWN_SECONDS)
        .await
        .unwrap();
    store.activate("ticket", "channel").await.unwrap();
    store.begin_close("ticket", 10).await.unwrap();
    sqlx::raw_sql(
        "ALTER TABLE ticket_transcripts ADD CONSTRAINT test_reject_body CHECK (content <> 'fail')",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let failed = store
        .save_transcript(
            "ticket",
            10,
            20,
            two_bot_core::tickets::TranscriptSnapshot {
                content: "fail".into(),
                message_count: 1,
            },
        )
        .await;
    assert!(matches!(failed, Err(StoreError::Database(_))));
    assert_eq!(
        store.get("ticket").await.unwrap().unwrap().status,
        TicketStatus::Closing
    );
    assert!(!store.transcript_exists("ticket").await.unwrap());
    store.reopen_interrupted("ticket", 10).await.unwrap();
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or a CI service container"]
async fn interrupted_create_and_legacy_saved_close_remain_recoverable() {
    let db = TestDb::new(false).await;
    let store = db.store("guild");
    store
        .reserve("creating", "creator", 0, COOLDOWN_SECONDS)
        .await
        .unwrap();
    assert_eq!(
        recovery_action(
            &store.get("creating").await.unwrap().unwrap(),
            INTERRUPTED_AFTER_MS,
            false
        ),
        RecoveryAction::FindCreatingChannel {
            topic: "two-ticket:creating".into()
        }
    );
    store.record_channel("creating", "orphan").await.unwrap();
    store.queue_open_rollback("creating", 20).await.unwrap();
    store.finish_cleanup("creating", 30).await.unwrap();
    assert!(!store.abandon_creating("creating").await.unwrap());
    store
        .reserve("legacy", "member", 0, COOLDOWN_SECONDS)
        .await
        .unwrap();
    store.activate("legacy", "channel").await.unwrap();
    store.begin_close("legacy", 10).await.unwrap();
    sqlx::query("INSERT INTO ticket_transcripts (ticket_id, guild_id, channel_id, opener_id, content, message_count, created_at, purge_after) VALUES ('legacy', 'guild', 'channel', 'member', 'durable', 1, '1970-01-01T00:00:00.020Z', '1970-04-01T00:00:00.020Z')").execute(&db.pool).await.unwrap();
    assert!(store.reopen_interrupted("legacy", 10).await.is_err());
    let recovered = store.recover_saved_close("legacy", 10).await.unwrap();
    assert_eq!(recovered.status, TicketStatus::CleanupPending);
    assert_eq!(recovered.closed_at, Some(20));
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or a CI service container"]
async fn purge_uses_90_day_boundary_and_all_operations_are_guild_fenced() {
    let db = TestDb::new(false).await;
    let a = db.store("a");
    let b = db.store("b");
    for (store, id, channel) in [(&a, "a-ticket", "a-channel"), (&b, "b-ticket", "b-channel")] {
        store
            .reserve(id, "member", 0, COOLDOWN_SECONDS)
            .await
            .unwrap();
        store.activate(id, channel).await.unwrap();
        store.begin_close(id, 10).await.unwrap();
        store
            .save_transcript(id, 10, 20, format_transcript(vec![]))
            .await
            .unwrap();
    }
    assert!(a.get("b-ticket").await.unwrap().is_none());
    assert!(a.by_channel("b-channel").await.unwrap().is_none());
    assert!(matches!(
        a.finish_cleanup("b-ticket", 30).await,
        Err(StoreError::NotFound)
    ));
    assert!(!a.transcript_exists("b-ticket").await.unwrap());
    let expiry = 20 + two_bot_core::tickets::TRANSCRIPT_RETENTION_MS;
    assert_eq!(a.purge_expired(expiry - 1).await.unwrap(), 0);
    assert_eq!(a.purge_expired(expiry).await.unwrap(), 1);
    assert!(!a.transcript_exists("a-ticket").await.unwrap());
    assert!(b.transcript_exists("b-ticket").await.unwrap());
    assert_eq!(a.erase_member("member").await.unwrap(), 1);
    assert_eq!(b.recoverable().await.unwrap().len(), 1);
    assert_eq!(b.erase_member("member").await.unwrap(), 1);
    assert!(!b.transcript_exists("b-ticket").await.unwrap());
    db.close().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb or a CI service container"]
async fn migration_upgrades_legacy_schema_without_losing_transcripts() {
    let db = TestDb::new(true).await;
    let (content, expiry): (String, String) = sqlx::query_as(
        "SELECT content, purge_after FROM ticket_transcripts WHERE ticket_id = 'old'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(content, "existing transcript");
    assert_eq!(expiry, "2026-12-07T12:00:00.000Z");
    let reserved = created(
        db.store("guild")
            .reserve("new", "new-member", 0, COOLDOWN_SECONDS)
            .await
            .unwrap(),
    );
    assert_eq!(reserved.channel_id, None);
    assert_eq!(reserved.status, TicketStatus::Creating);
    db.close().await;
}
