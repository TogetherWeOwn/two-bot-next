//! Explicit Postgres acceptance; never uses DATABASE_URL or operational secrets.
//! cargo test -p two-bot-core --features db --test audit_store --locked -- --ignored
#![cfg(feature = "db")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgPool, QueryBuilder};
use two_bot_core::audit::{delivery_nonce, AuditEvent, AuditKind};
use two_bot_core::audit_store::{
    AuditStore, DeliveryFailure, DeliveryIntent, DeliveryState, PrepareSend, QuarantineReason,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;
const MIGRATION: &str = include_str!("../../cutover/migrations/0340_operational_audit.sql");
static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

fn schema_name_at(nanos: u128) -> String {
    format!(
        "audit_test_{}_{}_{}",
        std::process::id(),
        NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

#[test]
fn schema_names_are_distinct_when_the_clock_repeats() {
    assert_ne!(schema_name_at(42), schema_name_at(42));
}

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    options: PgConnectOptions,
    schema: String,
}

impl TestDb {
    async fn new(migrate: bool) -> Result<Self, Box<dyn std::error::Error>> {
        // Fixed approved services, empty test password. Auth/ownership failures
        // propagate; no alternate credentials or fallback endpoints are tried.
        let host = if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
            "127.0.0.1"
        } else {
            "agent-testdb"
        };
        let options = PgConnectOptions::new()
            .host(host)
            .port(5432)
            .username("agent_test")
            .password("")
            .database("postgres")
            .ssl_mode(PgSslMode::Disable);
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.clone())
            .await?;
        let schema = schema_name_at(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos());
        QueryBuilder::<sqlx::Postgres>::new("CREATE SCHEMA ")
            .push(&schema)
            .build()
            .execute(&admin)
            .await?;
        // Tag every pool connection with the owned schema so contention tests
        // can attribute `pg_stat_activity` lock waits to this test alone when
        // other suites run against the same cluster in parallel.
        let options = options.application_name(&schema);
        let pool = Self::connect(&options, &schema).await?;
        if migrate {
            sqlx::raw_sql(MIGRATION).execute(&pool).await?;
        }
        Ok(Self {
            admin,
            pool,
            options,
            schema,
        })
    }

    async fn connect(options: &PgConnectOptions, schema: &str) -> Result<PgPool, sqlx::Error> {
        let path = schema.to_owned();
        PgPoolOptions::new()
            .max_connections(3)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |conn, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options.clone())
            .await
    }

    async fn peer(&self) -> Result<PgPool, sqlx::Error> {
        Self::connect(&self.options, &self.schema).await
    }

    async fn expire(&self, entry: &str) -> TestResult {
        sqlx::query("UPDATE operational_audit_log SET delivery_lease_until = clock_timestamp() - interval '1 second' WHERE entry_id = $1")
            .bind(entry).execute(&self.pool).await?;
        Ok(())
    }

    async fn finish(self) -> TestResult {
        self.pool.close().await;
        // Only the generated, run-owned schema is removed.
        QueryBuilder::<sqlx::Postgres>::new("DROP SCHEMA ")
            .push(&self.schema)
            .push(" CASCADE")
            .build()
            .execute(&self.admin)
            .await?;
        self.admin.close().await;
        Ok(())
    }
}

fn event(entry: &str) -> AuditEvent {
    let mut e = AuditEvent::new(
        entry.to_owned(),
        AuditKind::MemberUpdate,
        "18446744073709551615".to_owned(),
        "2026-09-30T00:01:02.003Z".to_owned(),
    );
    e.actor_id = Some("18446744073709551614".to_owned());
    e.target_id = Some("18446744073709551613".to_owned());
    e.metadata_json =
        r#"{"nicknameChanged":true,"addedRoleIds":["18446744073709551612"],"removedRoleIds":[]}"#
            .to_owned();
    e
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn replay_keeps_first_facts_and_destination_atomically() -> TestResult {
    let db = TestDb::new(true).await?;
    let store = AuditStore::new(&db.pool);
    let original = event("member-update:stable");
    assert!(store.record(&original, Some("123")).await?);
    let mut conflicting = original.clone();
    conflicting.guild_id = "999".to_owned();
    conflicting.metadata_json = r#"{"nicknameChanged":false}"#.to_owned();
    conflicting.target_id = None;
    assert!(!store.record(&conflicting, Some("456")).await?);
    let row = store.get(&original.entry_id).await?.unwrap();
    assert_eq!(row.event, original);
    assert_eq!(row.mirror_channel_id.as_deref(), Some("123"));
    assert_eq!(row.nonce, Some(delivery_nonce(&row.event.entry_id)));
    assert_eq!(row.state, DeliveryState::Pending);
    assert_eq!(row.attempts, 0);
    assert!(store.record(&event("store-only"), Some("")).await?);
    assert!(store.claim("store-only").await?.is_none());
    assert_eq!(
        store.get("store-only").await?.unwrap().state,
        DeliveryState::None
    );
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn concurrent_first_insert_and_two_workers_have_one_winner() -> TestResult {
    let db = TestDb::new(true).await?;
    let peer_pool = db.peer().await?;
    let a = AuditStore::new(&db.pool);
    let b = AuditStore::new(&peer_pool);
    let first = event("race");
    let second = event("race");
    let (left, right) = tokio::join!(
        a.record(&first, Some("123")),
        b.record(&second, Some("456"))
    );
    assert_ne!(left?, right?);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM operational_audit_log")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(count, 1);
    let (left, right) = tokio::join!(a.claim("race"), b.claim("race"));
    let left = left?;
    let right = right?;
    assert_ne!(left.is_some(), right.is_some());
    let winner = left.or(right).unwrap();
    assert_eq!(winner.intent(), DeliveryIntent::Send);
    assert!(a.release_unattempted(&winner).await?);
    assert!(!b.release_unattempted(&winner).await?);
    peer_pool.close().await;
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn stale_owner_cannot_ack_release_renew_or_change_a_newer_claim() -> TestResult {
    let db = TestDb::new(true).await?;
    let store = AuditStore::new(&db.pool);
    store.record(&event("fences"), Some("123")).await?;
    let old = store.claim("fences").await?.unwrap();
    db.expire("fences").await?;
    assert_eq!(store.prepare_send(&old, "0").await?, PrepareSend::LostClaim);
    assert!(!store.renew(&old).await?);
    let new = store.claim("fences").await?.unwrap();
    assert_eq!(store.prepare_send(&new, "0").await?, PrepareSend::Prepared);
    let before = store.get("fences").await?.unwrap();
    assert!(!store.release_unattempted(&old).await?);
    assert!(!store.note_accepted(&old, "234").await?);
    assert!(!store.complete(&old).await?);
    assert!(
        !store
            .fail_attempt(&old, DeliveryFailure::DefinitelyRejected)
            .await?
    );
    assert!(
        !store
            .quarantine(&old, QuarantineReason::MarkerMissing)
            .await?
    );
    assert_eq!(store.get("fences").await?.unwrap(), before);
    assert!(store.note_accepted(&new, "234").await?);
    assert!(store.complete(&new).await?);
    let delivered = store.get("fences").await?.unwrap();
    assert!(!store.note_accepted(&new, "345").await?);
    assert!(!store.complete(&new).await?);
    assert!(!store.release_unattempted(&new).await?);
    assert!(
        !store
            .fail_attempt(&new, DeliveryFailure::DefinitelyRejected)
            .await?
    );
    assert!(
        !store
            .quarantine(&new, QuarantineReason::EvidenceConflict)
            .await?
    );
    assert!(store.claim("fences").await?.is_none());
    assert_eq!(store.get("fences").await?.unwrap(), delivered);
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn crash_after_prepare_is_reconciliation_only_and_can_quarantine() -> TestResult {
    let db = TestDb::new(true).await?;
    let store = AuditStore::new(&db.pool);
    store.record(&event("prepared-crash"), Some("123")).await?;
    let old = store.claim("prepared-crash").await?.unwrap();
    assert_eq!(
        store.prepare_send(&old, "777").await?,
        PrepareSend::Prepared
    );
    assert_eq!(
        store.prepare_send(&old, "888").await?,
        PrepareSend::LostClaim
    );
    assert!(!store.release_unattempted(&old).await?);
    db.expire("prepared-crash").await?;
    let restart_pool = db.peer().await?;
    let restarted = AuditStore::new(&restart_pool);
    let recovery = restarted.claim("prepared-crash").await?.unwrap();
    assert_eq!(recovery.intent(), DeliveryIntent::Reconcile);
    assert_eq!(recovery.row().attempts, 1);
    assert_eq!(recovery.row().search_before.as_deref(), Some("777"));
    assert_eq!(
        restarted.prepare_send(&recovery, "999").await?,
        PrepareSend::LostClaim
    );
    assert!(
        !restarted
            .fail_attempt(&recovery, DeliveryFailure::DefinitelyRejected)
            .await?
    );
    assert!(!restarted.release_unattempted(&recovery).await?);
    assert!(!restarted.note_accepted(&old, "234").await?);
    assert!(!restarted.complete(&old).await?);
    assert!(
        !restarted
            .fail_attempt(&old, DeliveryFailure::DefinitelyRejected)
            .await?
    );
    assert!(
        restarted
            .quarantine(&recovery, QuarantineReason::MarkerMissing)
            .await?
    );
    let row = restarted.get("prepared-crash").await?.unwrap();
    assert_eq!(row.state, DeliveryState::Quarantined);
    assert_eq!(row.attempts, 1);
    assert_eq!(row.search_before.as_deref(), Some("777"));
    assert!(restarted.claim("prepared-crash").await?.is_none());
    restart_pool.close().await;
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn uncertain_acceptance_and_definite_rejection_are_distinct() -> TestResult {
    let db = TestDb::new(true).await?;
    let store = AuditStore::new(&db.pool);
    for id in ["uncertain", "rejected"] {
        store.record(&event(id), Some("123")).await?;
        let claim = store.claim(id).await?.unwrap();
        assert_eq!(
            store.prepare_send(&claim, "0").await?,
            PrepareSend::Prepared
        );
        assert!(
            store
                .fail_attempt(
                    &claim,
                    if id == "uncertain" {
                        DeliveryFailure::UncertainAcceptance
                    } else {
                        DeliveryFailure::DefinitelyRejected
                    }
                )
                .await?
        );
        let next = store.claim(id).await?.unwrap();
        assert_eq!(next.row().attempts, 1);
        assert_eq!(next.row().nonce, claim.row().nonce);
        if id == "uncertain" {
            assert_eq!(next.intent(), DeliveryIntent::Reconcile);
            assert_eq!(
                store.prepare_send(&next, "1").await?,
                PrepareSend::LostClaim
            );
            // Marker recovered by a Discord double/downstream reader, not POST.
            assert!(store.note_accepted(&next, "234").await?);
            assert!(store.complete(&next).await?);
        } else {
            assert_eq!(next.intent(), DeliveryIntent::Send);
            assert_eq!(store.prepare_send(&next, "1").await?, PrepareSend::Prepared);
            assert_eq!(store.get(id).await?.unwrap().attempts, 2);
        }
    }
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn accepted_evidence_survives_connection_restart_and_halt() -> TestResult {
    let mut db = TestDb::new(true).await?;
    let store = AuditStore::new(&db.pool);
    store.record(&event("accepted-crash"), Some("123")).await?;
    let claim = store.claim("accepted-crash").await?.unwrap();
    assert!(!store.note_accepted(&claim, "234").await?);
    assert!(!store.complete(&claim).await?);
    assert_eq!(
        store.prepare_send(&claim, "200").await?,
        PrepareSend::Prepared
    );
    assert!(store.note_accepted(&claim, "234").await?);
    assert!(!store.note_accepted(&claim, "345").await?);
    assert!(
        !store
            .fail_attempt(&claim, DeliveryFailure::DefinitelyRejected)
            .await?
    );
    let before = store.get("accepted-crash").await?.unwrap();
    assert!(before.accepted_at.is_some());
    assert!(store.engage_halt("456").await?);
    db.expire("accepted-crash").await?;
    db.pool.close().await;
    db.pool = db.peer().await?;
    let restarted = AuditStore::new(&db.pool);
    assert_eq!(restarted.get("accepted-crash").await?.unwrap(), before);
    assert!(restarted.delivery_halt().await?.is_some());
    assert!(restarted.claim("accepted-crash").await?.is_none());
    restarted.disengage_halt().await?;
    let recovery = restarted.claim("accepted-crash").await?.unwrap();
    assert_eq!(recovery.intent(), DeliveryIntent::Reconcile);
    assert_eq!(recovery.row().mirror_message_id.as_deref(), Some("234"));
    assert!(restarted.complete(&recovery).await?);
    let row = restarted.get("accepted-crash").await?.unwrap();
    assert_eq!(row.attempts, 1);
    assert_eq!(row.mirror_message_id, before.mirror_message_id);
    assert_eq!(row.accepted_at, before.accepted_at);
    assert_eq!(row.state, DeliveryState::Delivered);
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn halt_is_persistent_idempotent_and_checked_per_claim_and_prepare() -> TestResult {
    let db = TestDb::new(true).await?;
    let peer_pool = db.peer().await?;
    let store = AuditStore::new(&db.pool);
    let operator = AuditStore::new(&peer_pool);
    store.record(&event("halted"), Some("123")).await?;
    let claim = store.claim("halted").await?.unwrap();
    assert!(operator.engage_halt("456").await?);
    let first = operator.delivery_halt().await?;
    assert!(!operator.engage_halt("789").await?);
    assert_eq!(store.delivery_halt().await?, first);
    assert_eq!(store.prepare_send(&claim, "0").await?, PrepareSend::Halted);
    assert!(store.release_unattempted(&claim).await?);
    assert_eq!(store.get("halted").await?.unwrap().attempts, 0);
    assert!(store.pending_ids().await?.is_empty());
    assert!(store.claim("halted").await?.is_none());
    assert!(operator.disengage_halt().await?);
    assert!(!operator.disengage_halt().await?);
    assert_eq!(store.pending_ids().await?, vec!["halted"]);
    let resumed = store.claim("halted").await?.unwrap();
    assert_eq!(
        store.prepare_send(&resumed, "0").await?,
        PrepareSend::Prepared
    );
    // A switch pulled immediately after prepare is read by the downstream
    // pre-POST gate. Authoritative no-POST uses the same non-acceptance path.
    operator.engage_halt("456").await?;
    assert!(store.delivery_halt().await?.is_some());
    assert!(
        store
            .fail_attempt(&resumed, DeliveryFailure::DefinitelyRejected)
            .await?
    );
    assert_eq!(
        store.get("halted").await?.unwrap().state,
        DeliveryState::Pending
    );
    peer_pool.close().await;
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn queue_is_bounded_and_excludes_rota_and_terminal_rows() -> TestResult {
    let db = TestDb::new(true).await?;
    let store = AuditStore::new(&db.pool);
    for i in 0..30 {
        store
            .record(&event(&format!("queue-{i:02}")), Some("123"))
            .await?;
    }
    // Pre-existing legacy rota rows must never enter this audit API's queue.
    sqlx::query(
        "UPDATE operational_audit_log SET event_kind = 'rota_notice' WHERE entry_id = 'queue-00'",
    )
    .execute(&db.pool)
    .await?;
    let ids = store.pending_ids().await?;
    assert_eq!(ids.len(), 25);
    assert!(!ids.contains(&"queue-00".to_owned()));
    assert!(store.claim("queue-00").await?.is_none());
    assert!(store.get("queue-00").await.is_err());
    let claim = store.claim("queue-01").await?.unwrap();
    store
        .quarantine(&claim, QuarantineReason::PermissionRevoked)
        .await?;
    assert!(!store.pending_ids().await?.contains(&"queue-01".to_owned()));
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn failed_record_never_produces_a_send_or_a_partial_pending_row() -> TestResult {
    let db = TestDb::new(true).await?;
    let store = AuditStore::new(&db.pool);
    let mut invalid = event("bad-input");
    invalid.metadata_json = "[]".to_owned();
    assert!(store.record(&invalid, Some("123")).await.is_err());
    assert!(store.get("bad-input").await?.is_none());
    sqlx::query("ALTER TABLE operational_audit_log RENAME TO unavailable_audit")
        .execute(&db.pool)
        .await?;
    let mut sends = 0;
    // Consumer contract with a fake POST: an Err has no delivery branch.
    if store
        .record(&event("write-failure"), Some("123"))
        .await
        .is_ok()
    {
        sends += 1;
    }
    assert_eq!(sends, 0);
    assert!(store.claim("write-failure").await.is_err());
    sqlx::query("ALTER TABLE unavailable_audit RENAME TO operational_audit_log")
        .execute(&db.pool)
        .await?;
    assert!(store.get("write-failure").await?.is_none());
    assert!(store.claim("write-failure").await?.is_none());
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn switch_read_and_ack_write_errors_never_authorize_a_post() -> TestResult {
    use two_bot_core::audit::{KillSwitchLog, KillSwitchSnapshot};

    let db = TestDb::new(true).await?;
    let store = AuditStore::new(&db.pool);
    store.record(&event("switch-error"), Some("123")).await?;
    let claim = store.claim("switch-error").await?.unwrap();
    sqlx::query("ALTER TABLE audit_kill_switch RENAME TO unavailable_switch")
        .execute(&db.pool)
        .await?;
    let read = store.delivery_halt().await;
    assert!(read.is_err());
    let decision = KillSwitchSnapshot {
        observed_halted: None,
        halted: false,
        read_failed: read.is_err(),
    }
    .decide();
    assert!(!decision.halted);
    assert_eq!(decision.log, Some(KillSwitchLog::ReadFailed));
    // The legacy fail-open switch decision cannot repair failed preparation.
    assert!(store.prepare_send(&claim, "0").await.is_err());
    assert_eq!(store.get("switch-error").await?.unwrap().attempts, 0);
    assert!(store.release_unattempted(&claim).await?);
    sqlx::query("ALTER TABLE unavailable_switch RENAME TO audit_kill_switch")
        .execute(&db.pool)
        .await?;

    store.record(&event("ack-error"), Some("123")).await?;
    let sending = store.claim("ack-error").await?.unwrap();
    assert_eq!(
        store.prepare_send(&sending, "200").await?,
        PrepareSend::Prepared
    );
    // A fake Discord acceptance happened; then the acknowledgement DB is
    // unavailable. Never retry the POST; recover the already-durable boundary.
    sqlx::query("ALTER TABLE operational_audit_log RENAME TO unavailable_audit")
        .execute(&db.pool)
        .await?;
    assert!(store.note_accepted(&sending, "234").await.is_err());
    sqlx::query("ALTER TABLE unavailable_audit RENAME TO operational_audit_log")
        .execute(&db.pool)
        .await?;
    db.expire("ack-error").await?;
    let recovery = store.claim("ack-error").await?.unwrap();
    assert_eq!(recovery.intent(), DeliveryIntent::Reconcile);
    assert_eq!(recovery.row().attempts, 1);
    assert_eq!(recovery.row().search_before.as_deref(), Some("200"));
    assert_eq!(
        store.prepare_send(&recovery, "0").await?,
        PrepareSend::LostClaim
    );
    assert!(store.note_accepted(&recovery, "234").await?);
    assert!(store.complete(&recovery).await?);
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn embedded_runner_migrates_clean_schema_and_replays() -> TestResult {
    let db = TestDb::new(false).await?;
    let migrator = sqlx::migrate!("../cutover/migrations");
    migrator.run(&db.pool).await?;
    migrator.run(&db.pool).await?;
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&db.pool)
            .await?;
    assert!(versions.contains(&340));
    let store = AuditStore::new(&db.pool);
    assert!(store.record(&event("fresh-migration"), Some("123")).await?);
    sqlx::raw_sql(MIGRATION).execute(&db.pool).await?;
    assert_eq!(
        store.get("fresh-migration").await?.unwrap().state,
        DeliveryState::Pending
    );
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn populated_legacy_upgrade_preserves_inflight_recovery_and_halt() -> TestResult {
    let db = TestDb::new(false).await?;
    // Frozen legacy columns, not the new migration's DDL. No app DB touched.
    sqlx::raw_sql(
        "CREATE TABLE operational_audit_log (
          entry_id TEXT PRIMARY KEY, event_kind TEXT NOT NULL, guild_id TEXT NOT NULL,
          occurred_at TIMESTAMPTZ NOT NULL, actor_id TEXT, target_id TEXT,
          source_channel_id TEXT, destination_channel_id TEXT, message_id TEXT,
          action TEXT, metadata_json TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL,
          mirror_channel_id TEXT, delivery_state TEXT NOT NULL DEFAULT 'none',
          delivery_attempts INTEGER NOT NULL DEFAULT 0, delivery_attempted_at TIMESTAMPTZ,
          delivery_last_error TEXT, delivery_lease_until TIMESTAMPTZ, mirrored_at TIMESTAMPTZ,
          delivery_nonce TEXT, mirror_message_id TEXT, delivery_search_before TEXT,
          delivery_claim_token TEXT, mirror_checked_at TIMESTAMPTZ,
          delivery_generation BIGINT NOT NULL DEFAULT 0,
          delivery_accepted_at TIMESTAMPTZ);
         INSERT INTO operational_audit_log (entry_id,event_kind,guild_id,occurred_at,
          metadata_json,created_at,mirror_channel_id,delivery_state,delivery_attempts,
          delivery_lease_until,delivery_nonce,delivery_search_before,delivery_claim_token)
         VALUES ('legacy','message_delete','123',now(),'{}',now(),'234','delivering',3,
          now() - interval '1 hour','oa_existing','222','old-owner');
         CREATE TABLE audit_kill_switch (id INTEGER PRIMARY KEY, engaged_at TIMESTAMPTZ NOT NULL, engaged_by TEXT NOT NULL);
         INSERT INTO audit_kill_switch VALUES (1,now(),'456');",
    ).execute(&db.pool).await?;
    sqlx::raw_sql(MIGRATION).execute(&db.pool).await?;
    sqlx::raw_sql(MIGRATION).execute(&db.pool).await?;
    let store = AuditStore::new(&db.pool);
    assert!(store.claim("legacy").await?.is_none());
    assert_eq!(store.delivery_halt().await?.unwrap().engaged_by, "456");
    store.disengage_halt().await?;
    let claim = store.claim("legacy").await?.unwrap();
    assert_eq!(claim.intent(), DeliveryIntent::Reconcile);
    assert_eq!(claim.row().attempts, 3);
    assert_eq!(claim.row().nonce.as_deref(), Some("oa_existing"));
    assert_eq!(claim.row().search_before.as_deref(), Some("222"));
    assert_eq!(
        store.prepare_send(&claim, "0").await?,
        PrepareSend::LostClaim
    );
    assert!(store.note_accepted(&claim, "333").await?);
    assert!(store.complete(&claim).await?);
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn stale_owner_blocked_behind_unchanged_locker_cannot_mutate_after_expiry() -> TestResult {
    let db = TestDb::new(true).await?;
    let store = AuditStore::new(&db.pool);
    store.record(&event("lock-race"), Some("123")).await?;
    let owner = store.claim("lock-race").await?.unwrap();
    // One-second active lease: the owner mutation below must evaluate it only
    // after it holds the row lock.
    sqlx::query("UPDATE operational_audit_log SET delivery_lease_until = clock_timestamp() + interval '1 second' WHERE entry_id = 'lock-race'")
        .execute(&db.pool).await?;
    // An unrelated transaction parks on the row without changing it.
    let locker_pool = db.peer().await?;
    let mut locker = locker_pool.begin().await?;
    sqlx::query("SELECT 1 FROM operational_audit_log WHERE entry_id = 'lock-race' FOR UPDATE")
        .fetch_all(&mut *locker)
        .await?;
    // The owner mutation starts while the lease is active and blocks on the row.
    let worker = AuditStore::new(&db.pool);
    let claim = owner.clone();
    let renewing = tokio::spawn(async move { worker.renew(&claim).await });
    // Wait until the owner's connection is actually blocked on the row lock,
    // attributed to this test alone through the schema-tagged application name.
    let mut waited = 0;
    loop {
        let blocked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE application_name = $1 AND wait_event_type = 'Lock'",
        ).bind(&db.schema).fetch_one(&db.pool).await?;
        if blocked >= 1 {
            break;
        }
        if waited >= 100 {
            panic!("owner mutation never blocked on the row lock");
        }
        waited += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Hold the unchanged locker until the lease has expired.
    let mut expired = 0;
    loop {
        let done: bool = sqlx::query_scalar(
            "SELECT (delivery_lease_until <= clock_timestamp()) FROM operational_audit_log WHERE entry_id = 'lock-race'",
        ).fetch_one(&db.pool).await?;
        if done {
            break;
        }
        if expired >= 100 {
            panic!("lease never expired while blocked");
        }
        expired += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Release the row unchanged; the waiting owner must now see an expired lease.
    locker.commit().await?;
    assert!(!renewing.await.unwrap()?);
    assert_eq!(
        store.prepare_send(&owner, "0").await?,
        PrepareSend::LostClaim
    );
    assert!(!store.release_unattempted(&owner).await?);
    let row = store.get("lock-race").await?.unwrap();
    assert_eq!(row.state, DeliveryState::Delivering);
    assert_eq!(row.attempts, 0);
    assert!(row.search_before.is_none());
    locker_pool.close().await;
    db.finish().await
}

#[tokio::test]
#[ignore = "requires agent-testdb or CI Postgres service"]
async fn preflight_deferral_rotates_the_bounded_queue_without_counting_attempts() -> TestResult {
    let db = TestDb::new(true).await?;
    let store = AuditStore::new(&db.pool);
    for i in 0..26 {
        store
            .record(&event(&format!("defer-{i:02}")), Some("123"))
            .await?;
    }
    assert_eq!(store.pending_ids().await?.len(), 25);
    // Every row in the first batch hits a destination-tied preflight failure;
    // parking them must not count a POST attempt.
    for id in store.pending_ids().await? {
        let claim = store.claim(&id).await?.unwrap();
        assert!(store.defer_preflight(&claim).await?);
    }
    // The healthy 26th row is now the only discoverable candidate.
    assert_eq!(store.pending_ids().await?, vec!["defer-25".to_owned()]);
    for id in ["defer-00", "defer-01"] {
        assert_eq!(store.get(id).await?.unwrap().attempts, 0);
    }
    let healthy = store.claim("defer-25").await?.unwrap();
    assert_eq!(
        store.prepare_send(&healthy, "0").await?,
        PrepareSend::Prepared
    );
    // After the backoff expires the parked row is retryable, and the new claim
    // clears the deferral.
    sqlx::query("UPDATE operational_audit_log SET delivery_deferred_until = clock_timestamp() - interval '1 second' WHERE entry_id = 'defer-00'")
        .execute(&db.pool).await?;
    let retried = store.claim("defer-00").await?.unwrap();
    assert_eq!(retried.row().attempts, 0);
    let cleared: bool = sqlx::query_scalar(
        "SELECT delivery_deferred_until IS NULL FROM operational_audit_log WHERE entry_id = 'defer-00'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(cleared);
    assert_eq!(
        store.prepare_send(&retried, "1").await?,
        PrepareSend::Prepared
    );
    db.finish().await
}
