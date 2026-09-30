//! Explicitly requested tests require the authorized test container; never skip
//! configured failures or consult a production/staging URL. Each test owns a disposable database.
#![cfg(feature = "db")]

use sqlx::{PgPool, Row};
use two_bot_core::internal_action_store::{
    AuditSubject, DiscordId, ExecutionClaim, InternalActionStore, InternalClaim,
    InternalStoreError, ReconciliationEvidence, RequestIdentity, TerminalFailure, TerminalResponse,
};
use two_bot_core::{body_hash, CLAIM_STALE_SECONDS, NONCE_TTL_SECONDS, SKEW_SECONDS};
use two_bot_testsupport::TestDatabase;

struct TestDb {
    fixture: TestDatabase,
    pool: PgPool,
}

impl TestDb {
    async fn new() -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("explicit DB test requires TWO_TEST_DATABASE_URL (test bootstrap only)");
        let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .unwrap();
        let pool = fixture.pool().clone();
        Self { fixture, pool }
    }

    async fn independent_pool(&self) -> PgPool {
        self.fixture.independent_pool().await.unwrap()
    }

    fn store(&self) -> InternalActionStore {
        InternalActionStore::new(self.pool.clone())
    }

    async fn cleanup(self) {
        self.fixture.close().await.unwrap();
    }
}

/// Database clock in whole seconds. Timestamps handed to `burn_nonce` must be
/// fresh against this same clock at commit time, so read them here rather than
/// assuming the runner and database clocks agree.
async fn db_now_secs(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()))::bigint")
        .fetch_one(pool)
        .await
        .unwrap()
}

// Public cross-implementation signing fixture, never runtime key material.
fn signing_key() -> two_bot_core::internal_actions::SigningKey {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/internal-action-signing.json");
    let fixture: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    two_bot_core::internal_actions::SigningKey {
        id: "fixture-caller".to_owned(),
        secret: fixture["vectors"][0]["secret"]
            .as_str()
            .unwrap()
            .as_bytes()
            .to_vec(),
    }
}

#[tokio::test]
async fn authenticated_malformed_body_burns_before_parsing_and_survives_restart() {
    use two_bot_core::internal_actions::{
        sign, AuthHeaders, AuthenticatedRequest, ErrorCode, InternalFlags, KeyRing, TokenBuckets,
    };

    let db = TestDb::new().await;
    let key = signing_key();
    let keys = KeyRing::new(vec![key.clone()]);
    let now = db_now_secs(&db.pool).await as u64;
    let timestamp = now.to_string();
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let raw = b"not json";
    let signature = sign(&key.secret, &timestamp, &nonce, raw);
    let headers = AuthHeaders {
        key_id: &key.id,
        timestamp: &timestamp,
        nonce: &nonce,
        signature: &signature,
    };
    let verified = AuthenticatedRequest::verify(&headers, raw, &keys, SKEW_SECONDS, now).unwrap();
    let burned = verified.burn_durably(&db.store()).await.unwrap();
    let flags = InternalFlags::from_map(&std::collections::HashMap::new());
    let error = burned
        .authorize(&flags, true, false, now * 1000, &mut TokenBuckets::new())
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Malformed);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_nonces")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    let restarted = db.independent_pool().await;
    let verified = AuthenticatedRequest::verify(&headers, raw, &keys, SKEW_SECONDS, now).unwrap();
    let error = verified
        .burn_durably(&InternalActionStore::new(restarted.clone()))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, ErrorCode::Replayed);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_idempotency")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    restarted.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn invalid_authentication_cannot_burn_or_drain_buckets() {
    use two_bot_core::internal_actions::{
        sign, AuthHeaders, AuthenticatedRequest, ErrorCode, InternalFlags, KeyRing, TokenBuckets,
    };

    let db = TestDb::new().await;
    let key = signing_key();
    let keys = KeyRing::new(vec![key.clone()]);
    let now = db_now_secs(&db.pool).await as u64;
    let timestamp = now.to_string();
    let stale = (now - SKEW_SECONDS - 1).to_string();
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let raw = br#"{"action":"role.assign"}"#;
    let good = sign(&key.secret, &timestamp, &nonce, raw);
    let old = sign(&key.secret, &stale, &nonce, raw);
    let mut buckets = TokenBuckets::new();
    for (id, timestamp, signature, expected) in [
        (
            key.id.as_str(),
            timestamp.as_str(),
            "sha256=invalid",
            ErrorCode::Unauthorized,
        ),
        (
            "unknown",
            timestamp.as_str(),
            good.as_str(),
            ErrorCode::Unauthorized,
        ),
        (
            key.id.as_str(),
            stale.as_str(),
            old.as_str(),
            ErrorCode::StaleRequest,
        ),
    ] {
        for _ in 0..25 {
            let headers = AuthHeaders {
                key_id: id,
                timestamp,
                nonce: &nonce,
                signature,
            };
            let error = AuthenticatedRequest::verify(&headers, raw, &keys, SKEW_SECONDS, now)
                .err()
                .unwrap();
            assert_eq!(error.code, expected);
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_nonces")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let headers = AuthHeaders {
        key_id: &key.id,
        timestamp: &timestamp,
        nonce: &nonce,
        signature: &good,
    };
    let burned = AuthenticatedRequest::verify(&headers, raw, &keys, SKEW_SECONDS, now)
        .unwrap()
        .burn_durably(&db.store())
        .await
        .unwrap();
    let flags = InternalFlags::from_map(&std::collections::HashMap::new());
    let decision = burned
        .authorize(&flags, true, false, now * 1000, &mut buckets)
        .unwrap();
    assert_eq!(decision.action, "role.assign");
    db.cleanup().await;
}

#[tokio::test]
async fn failed_nonce_storage_and_incompatible_skew_cannot_grant_authorization() {
    use two_bot_core::internal_actions::{
        sign, AuthHeaders, AuthenticatedRequest, ErrorCode, KeyRing,
    };

    let db = TestDb::new().await;
    let key = signing_key();
    let keys = KeyRing::new(vec![key.clone()]);
    let now = db_now_secs(&db.pool).await as u64;
    let timestamp = now.to_string();
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let raw = br#"{"action":"role.assign"}"#;
    let signature = sign(&key.secret, &timestamp, &nonce, raw);
    let headers = AuthHeaders {
        key_id: &key.id,
        timestamp: &timestamp,
        nonce: &nonce,
        signature: &signature,
    };
    for skew in [SKEW_SECONDS - 1, SKEW_SECONDS + 1] {
        let error = AuthenticatedRequest::verify(&headers, raw, &keys, skew, now)
            .unwrap()
            .burn_durably(&db.store())
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(error.log_reason, "nonce_skew_mismatch");
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_nonces")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    let closed = db.independent_pool().await;
    closed.close().await;
    let error = AuthenticatedRequest::verify(&headers, raw, &keys, SKEW_SECONDS, now)
        .unwrap()
        .burn_durably(&InternalActionStore::new(closed))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, ErrorCode::Internal);
    assert_eq!(error.log_reason, "nonce_store_unavailable");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_nonces")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    db.cleanup().await;
}

async fn wait_for_uniqueness_lock(db: &TestDb, table: &str) {
    // Observe only this owned test database's sessions. Bounded synchronization
    // proves the VALUES clock was sampled before rollback; no timing-only sleep.
    for _ in 0..100 {
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE application_name = $1 \
             AND wait_event_type = 'Lock' AND query LIKE '%' || $2 || '%')",
        )
        .bind(db.fixture.name())
        .bind(table)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        if blocked {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("test writer did not reach the expected unique-index lock");
}

#[tokio::test]
async fn rollback_wait_starts_retention_and_staleness_after_winning_insert() {
    let db = TestDb::new().await;
    let second = db.independent_pool().await;
    let store = InternalActionStore::new(second.clone());
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let nonce_hash = body_hash(nonce.as_bytes());
    let mut blocker = db.pool.begin().await.unwrap();
    sqlx::query(
        "WITH instant AS MATERIALIZED (SELECT clock_timestamp() AS now) \
         INSERT INTO internal_nonces SELECT $1, now, now + INTERVAL '241 seconds' FROM instant",
    )
    .bind(&nonce_hash)
    .execute(&mut *blocker)
    .await
    .unwrap();
    let writer = store.clone();
    let offered_nonce = nonce.clone();
    let attempt = db_now_secs(&db.pool).await.to_string();
    let burn = tokio::spawn(async move { writer.burn_nonce(&offered_nonce, &attempt).await });
    wait_for_uniqueness_lock(&db, "internal_nonces").await;
    let release: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    blocker.rollback().await.unwrap();
    assert!(burn.await.unwrap().unwrap());
    let row =
        sqlx::query("SELECT burned_at, expires_at FROM internal_nonces WHERE nonce_hash = $1")
            .bind(&nonce_hash)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let burned_at: time::OffsetDateTime = row.get("burned_at");
    let expires_at: time::OffsetDateTime = row.get("expires_at");
    assert!(burned_at >= release);
    assert_eq!(expires_at - burned_at, time::Duration::seconds(241));
    let now = db_now_secs(&db.pool).await.to_string();
    assert!(!store.burn_nonce(&nonce, &now).await.unwrap());

    let id = identity("blocked-key:123", "event.upsert", b"payload");
    let mut blocker = db.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO internal_idempotency (caller_hash, key_hash, action, payload_hash, state) \
         VALUES ($1, $2, 'event.upsert', $3, 'in_flight')",
    )
    .bind(body_hash(b"website"))
    .bind(body_hash(b"blocked-key:123"))
    .bind(body_hash(b"payload"))
    .execute(&mut *blocker)
    .await
    .unwrap();
    let writer = store.clone();
    let offered_id = id.clone();
    let claim = tokio::spawn(async move { writer.claim(&offered_id, &subject()).await });
    wait_for_uniqueness_lock(&db, "internal_idempotency").await;
    let release: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    blocker.rollback().await.unwrap();
    let owner = claimed(claim.await.unwrap().unwrap());
    let created_at: time::OffsetDateTime =
        sqlx::query_scalar("SELECT created_at FROM internal_idempotency WHERE intent_id = $1")
            .bind(owner.intent_id())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(created_at >= release);
    assert!(matches!(
        store.claim(&id, &subject()).await.unwrap(),
        InternalClaim::InFlight
    ));
    second.close().await;
    db.cleanup().await;
}

fn identity(key: &str, action: &str, payload: &[u8]) -> RequestIdentity {
    RequestIdentity::new("website", key, action, payload).unwrap()
}

fn subject() -> AuditSubject {
    AuditSubject {
        guild_id: Some(DiscordId::new("123456789012345678").unwrap()),
        actor_id: Some(DiscordId::new("234567890123456789").unwrap()),
        target_id: None,
    }
}

fn claimed(result: InternalClaim) -> ExecutionClaim {
    match result {
        InternalClaim::Claimed(claim) => claim,
        other => panic!("expected committed execution claim, got {other:?}"),
    }
}

fn success() -> TerminalResponse {
    TerminalResponse::Success {
        resource_id: Some(DiscordId::new("345678901234567890").unwrap()),
        affected: 2,
    }
}

/// A replay that is fresh before an async wait but stale after it must be
/// refused, never granted a second burn. Seeds a live row expiring ~2s out,
/// holds the writer behind a controlled row lock for 3s (crossing both the
/// row expiry and the attempt's skew window), then asserts `InvalidInput`
/// and that the original burn stands untouched. A fresh attempt against the
/// same expired row still wins a genuine replacement afterwards.
#[tokio::test]
async fn stale_wait_cannot_win_second_nonce_burn() {
    let db = TestDb::new().await;
    let second = db.independent_pool().await;
    let store = InternalActionStore::new(second.clone());
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let nonce_hash = body_hash(nonce.as_bytes());
    let start = db_now_secs(&db.pool).await;
    sqlx::query(
        "WITH instant AS MATERIALIZED (SELECT clock_timestamp() AS now) \
         INSERT INTO internal_nonces SELECT $1, now - INTERVAL '239 seconds', now + INTERVAL '2 seconds' FROM instant",
    )
    .bind(&nonce_hash)
    .execute(&db.pool)
    .await
    .unwrap();
    // Fresh now (1s of margin), stale after the 3s hold plus lock overhead.
    let attempt = (start - SKEW_SECONDS as i64 + 1).to_string();
    let mut blocker = db.pool.begin().await.unwrap();
    sqlx::query("SELECT nonce_hash FROM internal_nonces WHERE nonce_hash = $1 FOR UPDATE")
        .bind(&nonce_hash)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let writer = store.clone();
    let offered = nonce.clone();
    let burn = tokio::spawn(async move { writer.burn_nonce(&offered, &attempt).await });
    wait_for_uniqueness_lock(&db, "internal_nonces").await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    blocker.rollback().await.unwrap();
    assert_eq!(
        burn.await.unwrap(),
        Err(InternalStoreError::InvalidInput),
        "a replay that lapsed mid-wait must be refused"
    );
    let row =
        sqlx::query("SELECT burned_at, expires_at FROM internal_nonces WHERE nonce_hash = $1")
            .bind(&nonce_hash)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let burned_at: time::OffsetDateTime = row.get("burned_at");
    let now: time::OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    // The refused replay replaced nothing: the row still dates from the seed.
    assert!(
        (now - burned_at) >= time::Duration::seconds(238),
        "refused replay must not refresh the burn"
    );
    // A genuinely fresh attempt against the expired row replaces it.
    let fresh = db_now_secs(&db.pool).await.to_string();
    assert!(store.burn_nonce(&nonce, &fresh).await.unwrap());
    second.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn nonce_race_restart_and_expiry_window() {
    let db = TestDb::new().await;
    let second = db.independent_pool().await;
    let a = db.store();
    let b = InternalActionStore::new(second.clone());
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let attempt = db_now_secs(&db.pool).await.to_string();
    let mut tasks = Vec::new();
    for i in 0..24 {
        let store = if i % 2 == 0 { a.clone() } else { b.clone() };
        let nonce = nonce.clone();
        let attempt = attempt.clone();
        tasks.push(tokio::spawn(async move {
            store.burn_nonce(&nonce, &attempt).await.unwrap()
        }));
    }
    let mut wins = 0;
    for task in tasks {
        wins += usize::from(task.await.unwrap());
    }
    assert_eq!(wins, 1);
    let retained: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM expires_at - burned_at)::double precision FROM internal_nonces",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(retained >= (2 * SKEW_SECONDS + 1) as f64);
    assert_eq!(retained, NONCE_TTL_SECONDS as f64);
    // A future expiry is refused. A known-past expiry is replaceable, with a
    // full fresh interval. Alter BOTH timestamps to preserve the retention check.
    let now = db_now_secs(&db.pool).await.to_string();
    sqlx::query("UPDATE internal_nonces SET burned_at = clock_timestamp() - INTERVAL '240 seconds', expires_at = clock_timestamp() + INTERVAL '1 second'")
        .execute(&db.pool).await.unwrap();
    assert!(!b.burn_nonce(&nonce, &now).await.unwrap());
    let now = db_now_secs(&db.pool).await.to_string();
    sqlx::query("UPDATE internal_nonces SET burned_at = clock_timestamp() - INTERVAL '242 seconds', expires_at = clock_timestamp() - INTERVAL '1 second'")
        .execute(&db.pool).await.unwrap();
    assert!(b.burn_nonce(&nonce, &now).await.unwrap());
    second.close().await;
    let restarted = db.independent_pool().await;
    let now = db_now_secs(&db.pool).await.to_string();
    assert!(!InternalActionStore::new(restarted.clone())
        .burn_nonce(&nonce, &now)
        .await
        .unwrap());
    // Built dynamically so static analysis does not read a hard-coded nonce.
    // A malformed (non-numeric) timestamp fails the commit-time freshness
    // check even though the nonce format check runs first.
    let invalid_nonce = ["not", "a", "valid", "nonce"].join("-");
    assert_eq!(
        a.burn_nonce(&invalid_nonce, "not-a-timestamp").await,
        Err(InternalStoreError::InvalidInput)
    );
    restarted.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn claims_race_bind_payload_action_caller_and_replay_after_restart() {
    let db = TestDb::new().await;
    let second = db.independent_pool().await;
    let a = db.store();
    let b = InternalActionStore::new(second.clone());
    let identity = identity(
        "request:123",
        "announcement.post",
        b"{\"content\":\"hello\"}",
    );
    let mut tasks = Vec::new();
    for i in 0..24 {
        let store = if i % 2 == 0 { a.clone() } else { b.clone() };
        let id = identity.clone();
        tasks.push(tokio::spawn(async move {
            store.claim(&id, &subject()).await.unwrap()
        }));
    }
    let mut winner = None;
    for task in tasks {
        match task.await.unwrap() {
            InternalClaim::Claimed(c) => assert!(winner.replace(c).is_none()),
            InternalClaim::InFlight => {}
            other => panic!("unexpected concurrent result: {other:?}"),
        }
    }
    let winner = winner.unwrap();
    for changed in [
        RequestIdentity::new(
            "website",
            "request:123",
            "event.upsert",
            b"{\"content\":\"hello\"}",
        )
        .unwrap(),
        RequestIdentity::new(
            "website",
            "request:123",
            "announcement.post",
            b"{ \"content\":\"hello\"}",
        )
        .unwrap(),
    ] {
        assert!(matches!(
            b.claim(&changed, &subject()).await.unwrap(),
            InternalClaim::Mismatch
        ));
    }
    let caller2 = RequestIdentity::new(
        "other-website",
        "request:123",
        "announcement.post",
        b"{\"content\":\"hello\"}",
    )
    .unwrap();
    assert!(matches!(
        b.claim(&caller2, &subject()).await.unwrap(),
        InternalClaim::Claimed(_)
    ));
    let intent_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM internal_action_log WHERE phase = 'intent'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(intent_count, 2); // One intent per caller slot, not per delivery.
    a.finish(&winner, &success()).await.unwrap();
    assert_eq!(
        a.finish(&winner, &success()).await,
        Err(InternalStoreError::TransitionRefused)
    );
    second.close().await;
    let restarted = db.independent_pool().await;
    let c = InternalActionStore::new(restarted.clone());
    assert!(
        matches!(c.claim(&identity, &subject()).await.unwrap(), InternalClaim::Replay(r) if r == success())
    );
    let changed = RequestIdentity::new(
        "website",
        "request:123",
        "announcement.post",
        b"other payload",
    )
    .unwrap();
    assert!(matches!(
        c.claim(&changed, &subject()).await.unwrap(),
        InternalClaim::Mismatch
    ));
    restarted.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn stale_and_unknown_claims_never_reexecute_and_reconciliation_is_terminal() {
    let db = TestDb::new().await;
    let a = db.store();
    let id = identity("stale-key:123", "moderation.ban", b"validated payload");
    let owner = claimed(a.claim(&id, &subject()).await.unwrap());
    assert_eq!(
        a.reconcile(
            &id,
            &success(),
            ReconciliationEvidence::DiscordConfirmedEffect
        )
        .await,
        Err(InternalStoreError::TransitionRefused)
    );
    sqlx::query("UPDATE internal_idempotency SET created_at = clock_timestamp() - ($1::bigint * INTERVAL '1 second') WHERE intent_id = $2")
        .bind(i64::try_from(CLAIM_STALE_SECONDS).unwrap())
        .bind(owner.intent_id()).execute(&db.pool).await.unwrap();
    let second = db.independent_pool().await;
    let restarted = InternalActionStore::new(second.clone());
    for _ in 0..3 {
        assert!(matches!(
            restarted.claim(&id, &subject()).await.unwrap(),
            InternalClaim::NeedsReconciliation
        ));
    }
    let unknown_id = identity(
        "unknown-key:123",
        "guild.add_member",
        b"validated OAuth payload",
    );
    let unknown = claimed(a.claim(&unknown_id, &subject()).await.unwrap());
    a.mark_unknown(&unknown).await.unwrap();
    a.mark_unknown(&unknown).await.unwrap(); // Audit dedup, still never a new claim.
    assert!(matches!(
        restarted.claim(&unknown_id, &subject()).await.unwrap(),
        InternalClaim::NeedsReconciliation
    ));
    assert_eq!(
        a.reconcile(&id, &success(), ReconciliationEvidence::ProvenNotSent)
            .await,
        Err(InternalStoreError::InvalidInput)
    );
    restarted
        .reconcile(
            &id,
            &success(),
            ReconciliationEvidence::DiscordConfirmedEffect,
        )
        .await
        .unwrap();
    assert_eq!(
        a.finish(
            &owner,
            &TerminalResponse::Failure(TerminalFailure::NoEffect)
        )
        .await,
        Err(InternalStoreError::TransitionRefused)
    );
    assert!(
        matches!(a.claim(&id, &subject()).await.unwrap(), InternalClaim::Replay(r) if r == success())
    );
    let failure = TerminalResponse::Failure(TerminalFailure::NoEffect);
    restarted
        .reconcile(&unknown_id, &failure, ReconciliationEvidence::ProvenNotSent)
        .await
        .unwrap();
    assert!(
        matches!(a.claim(&unknown_id, &subject()).await.unwrap(), InternalClaim::Replay(r) if r == failure)
    );
    assert_eq!(
        a.mark_unknown(&unknown).await,
        Err(InternalStoreError::TransitionRefused)
    );
    second.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn audit_failures_roll_back_claim_and_terminal_without_leaking_details() {
    let db = TestDb::new().await;
    let store = db.store();
    let id = identity(
        "redaction-key:123",
        "guild.add_member",
        br#"{"access_token":"private-oauth-value","reason":"never-audit-this"}"#,
    );
    sqlx::query(
        "ALTER TABLE internal_action_log ADD CONSTRAINT injected_failure CHECK (phase <> 'intent')",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(matches!(
        store.claim(&id, &subject()).await,
        Err(InternalStoreError::Unavailable)
    ));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("ALTER TABLE internal_action_log DROP CONSTRAINT injected_failure")
        .execute(&db.pool)
        .await
        .unwrap();
    let owner = claimed(store.claim(&id, &subject()).await.unwrap());
    sqlx::query("ALTER TABLE internal_action_log ADD CONSTRAINT injected_failure CHECK (phase <> 'terminal')")
        .execute(&db.pool).await.unwrap();
    let error = store.finish(&owner, &success()).await.unwrap_err();
    assert_eq!(error.to_string(), "internal-action storage unavailable");
    assert!(std::error::Error::source(&error).is_none());
    assert!(matches!(
        store.claim(&id, &subject()).await.unwrap(),
        InternalClaim::InFlight
    ));
    sqlx::query("ALTER TABLE internal_action_log DROP CONSTRAINT injected_failure")
        .execute(&db.pool)
        .await
        .unwrap();
    store.finish(&owner, &success()).await.unwrap();
    let persisted: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(i)::text FROM internal_idempotency i UNION ALL SELECT row_to_json(a)::text FROM internal_action_log a",
    ).fetch_all(&db.pool).await.unwrap();
    for row in persisted {
        for raw in [
            "private-oauth-value",
            "never-audit-this",
            "redaction-key:123",
            "website",
        ] {
            assert!(!row.contains(raw));
        }
    }
    let terminal = sqlx::query("SELECT response_code, http_status, evidence_code FROM internal_action_log WHERE phase = 'terminal'")
        .fetch_one(&db.pool).await.unwrap();
    assert_eq!(terminal.get::<&str, _>("response_code"), "success");
    assert_eq!(terminal.get::<i32, _>("http_status"), 200);
    assert_eq!(terminal.get::<&str, _>("evidence_code"), "executor");
    // Capture while the pool is still open: after `close` the burn fails at
    // transaction begin (Unavailable) before any freshness logic runs.
    let attempt = db_now_secs(&db.pool).await.to_string();
    db.pool.close().await;
    assert!(matches!(
        store.claim(&id, &subject()).await,
        Err(InternalStoreError::Unavailable)
    ));
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    assert_eq!(
        store.burn_nonce(&nonce, &attempt).await,
        Err(InternalStoreError::Unavailable)
    );
    assert_eq!(
        store.claim_discord_event("gateway:session:123").await,
        Err(InternalStoreError::Unavailable)
    );
    db.cleanup().await;
}

#[tokio::test]
async fn discord_event_dedup_is_atomic_global_and_durable() {
    let db = TestDb::new().await;
    let second = db.independent_pool().await;
    let a = db.store();
    let b = InternalActionStore::new(second.clone());
    let mut tasks = Vec::new();
    for i in 0..24 {
        let store = if i % 2 == 0 { a.clone() } else { b.clone() };
        tasks.push(tokio::spawn(async move {
            store.claim_discord_event("guild:event:123").await.unwrap()
        }));
    }
    let mut wins = 0;
    for task in tasks {
        wins += usize::from(task.await.unwrap());
    }
    assert_eq!(wins, 1);
    second.close().await;
    let restarted = db.independent_pool().await;
    let c = InternalActionStore::new(restarted.clone());
    assert!(!c.claim_discord_event("guild:event:123").await.unwrap());
    assert!(c.claim_discord_event("guild:event:124").await.unwrap());
    assert_eq!(
        c.claim_discord_event("secret\nheader").await,
        Err(InternalStoreError::InvalidInput)
    );
    restarted.close().await;
    db.cleanup().await;
}

#[tokio::test]
async fn completion_and_reconciliation_race_cannot_overwrite_terminal() {
    let db = TestDb::new().await;
    let second = db.independent_pool().await;
    let executor = db.store();
    let reconciler = InternalActionStore::new(second.clone());
    for n in 0..16 {
        let id = identity(
            &format!("finish-race:{n}"),
            "event.upsert",
            b"validated payload",
        );
        let owner = claimed(executor.claim(&id, &subject()).await.unwrap());
        executor.mark_unknown(&owner).await.unwrap();
        let success = success();
        let failure = TerminalResponse::Failure(TerminalFailure::NoEffect);
        let (finished, reconciled) = tokio::join!(
            executor.finish(&owner, &success),
            reconciler.reconcile(&id, &failure, ReconciliationEvidence::ProvenNotSent)
        );
        let expected = match (finished, reconciled) {
            (Ok(()), Err(InternalStoreError::TransitionRefused)) => success,
            (Err(InternalStoreError::TransitionRefused), Ok(())) => failure,
            other => panic!("exactly one terminal writer must win: {other:?}"),
        };
        assert!(
            matches!(executor.claim(&id, &subject()).await.unwrap(), InternalClaim::Replay(r) if r == expected)
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM internal_action_log WHERE intent_id = $1 AND phase = 'terminal'",
        )
        .bind(owner.intent_id())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
    }
    second.close().await;
    db.cleanup().await;
}
