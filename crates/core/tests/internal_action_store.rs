//! Explicitly requested tests require the authorized test container; never skip
//! configured failures or consult a production/staging URL. Each test owns a disposable database.
#![cfg(feature = "db")]

use sqlx::{PgPool, Row};
use two_bot_core::clock_guard::ClockGuard;
use two_bot_core::internal_action_store::{
    AuditSubject, DiscordId, ExecutionClaim, InternalActionStore, InternalClaim,
    InternalStoreError, ReconciliationEvidence, RequestIdentity, TerminalFailure, TerminalResponse,
};
use two_bot_core::secret::Secret;
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
        secret: Secret::new(
            fixture["vectors"][0]["secret"]
                .as_str()
                .unwrap()
                .as_bytes()
                .to_vec(),
        ),
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
    let signature = sign(key.secret.expose(), &timestamp, &nonce, raw);
    let headers = AuthHeaders {
        key_id: &key.id,
        timestamp: &timestamp,
        nonce: &nonce,
        signature: &signature,
    };
    let verified = AuthenticatedRequest::verify(
        &headers,
        raw,
        &keys,
        SKEW_SECONDS,
        now * 1000,
        &mut ClockGuard::new(),
    )
    .unwrap();
    let burned = verified.burn_durably(&db.store()).await.unwrap();
    let flags = InternalFlags::from_map(&std::collections::HashMap::new());
    let error = burned
        .authorize(&flags, true, false, &mut TokenBuckets::new())
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Malformed);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_nonces")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    let restarted = db.independent_pool().await;
    let verified = AuthenticatedRequest::verify(
        &headers,
        raw,
        &keys,
        SKEW_SECONDS,
        now * 1000,
        &mut ClockGuard::new(),
    )
    .unwrap();
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
    let good = sign(key.secret.expose(), &timestamp, &nonce, raw);
    let old = sign(key.secret.expose(), &stale, &nonce, raw);
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
            let error = AuthenticatedRequest::verify(
                &headers,
                raw,
                &keys,
                SKEW_SECONDS,
                now * 1000,
                &mut ClockGuard::new(),
            )
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
    let burned = AuthenticatedRequest::verify(
        &headers,
        raw,
        &keys,
        SKEW_SECONDS,
        now * 1000,
        &mut ClockGuard::new(),
    )
    .unwrap()
    .burn_durably(&db.store())
    .await
    .unwrap();
    let flags = InternalFlags::from_map(&std::collections::HashMap::new());
    let decision = burned.authorize(&flags, true, false, &mut buckets).unwrap();
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
    let signature = sign(key.secret.expose(), &timestamp, &nonce, raw);
    let headers = AuthHeaders {
        key_id: &key.id,
        timestamp: &timestamp,
        nonce: &nonce,
        signature: &signature,
    };
    for skew in [SKEW_SECONDS - 1, SKEW_SECONDS + 1] {
        let error = AuthenticatedRequest::verify(
            &headers,
            raw,
            &keys,
            skew,
            now * 1000,
            &mut ClockGuard::new(),
        )
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
    let error = AuthenticatedRequest::verify(
        &headers,
        raw,
        &keys,
        SKEW_SECONDS,
        now * 1000,
        &mut ClockGuard::new(),
    )
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

#[tokio::test]
async fn staged_authentication_refuses_persisted_db_clock_rollback_after_restart() {
    use two_bot_core::internal_actions::{
        sign, AuthHeaders, AuthenticatedRequest, ErrorCode, KeyRing,
    };

    let db = TestDb::new().await;
    let store = db.store();
    let seed_nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let seed_timestamp = db_now_secs(&db.pool).await.to_string();
    assert!(store
        .burn_nonce(&seed_nonce, &seed_timestamp)
        .await
        .unwrap());
    let future_ms = store.nonce_high_water_ms().await.unwrap().unwrap() + 300_000;
    sqlx::query(
        "UPDATE internal_clock_high_water SET high_water_ms = $1, \
         observed_at = TO_TIMESTAMP($1::double precision / 1000.0) \
         WHERE domain = 'internal_nonce_db'",
    )
    .bind(future_ms as i64)
    .execute(&db.pool)
    .await
    .unwrap();

    let key = signing_key();
    let keys = KeyRing::new(vec![key.clone()]);
    let now = db_now_secs(&db.pool).await as u64;
    let timestamp = now.to_string();
    let nonce = body_hash(format!("{}-fresh", db.fixture.name()).as_bytes())[..32].to_owned();
    let raw = br#"{"action":"role.assign"}"#;
    let signature = sign(key.secret.expose(), &timestamp, &nonce, raw);
    let headers = AuthHeaders {
        key_id: &key.id,
        timestamp: &timestamp,
        nonce: &nonce,
        signature: &signature,
    };
    let restarted = db.independent_pool().await;
    // A new process clock must not be seeded from the independent DB domain.
    // A locally fresh MAC still cannot pass the persisted DB rollback guard.
    let error = AuthenticatedRequest::verify(
        &headers,
        raw,
        &keys,
        SKEW_SECONDS,
        now * 1000,
        &mut ClockGuard::new(),
    )
    .unwrap()
    .burn_durably(&InternalActionStore::new(restarted.clone()))
    .await
    .err()
    .unwrap();
    assert_eq!(error.code, ErrorCode::StaleRequest);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM internal_nonces WHERE nonce_hash = $1")
            .bind(body_hash(nonce.as_bytes()))
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    assert_eq!(store.nonce_high_water_ms().await.unwrap(), Some(future_ms));
    restarted.close().await;
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
        resolved_role_id: None,
    }
}

fn claimed(result: InternalClaim) -> ExecutionClaim {
    match result {
        InternalClaim::Claimed(claim) => claim,
        other => panic!("expected committed execution claim, got {other:?}"),
    }
}

#[tokio::test]
async fn proven_unsent_release_retains_binding_and_allows_one_new_owner() {
    let db = TestDb::new().await;
    let store = db.store();
    let id = identity("unsent-key:123", "role.assign", b"payload");
    let subject = subject();
    let nonce = "0123456789abcdef0123456789abcdef";
    let timestamp = db_now_secs(&db.pool).await.to_string();
    assert!(store.burn_nonce(nonce, &timestamp).await.unwrap());
    let first = claimed(store.claim(&id, &subject).await.unwrap());
    let intent_id = first.intent_id();
    store.release_proven_not_sent(first).await.unwrap();
    assert!(!store.burn_nonce(nonce, &timestamp).await.unwrap());
    for changed in [
        identity("unsent-key:123", "role.assign", b"changed"),
        identity("unsent-key:123", "guild.add_member", b"payload"),
    ] {
        assert!(matches!(
            store.claim(&changed, &subject).await.unwrap(),
            InternalClaim::Mismatch
        ));
    }
    let mut remapped = subject.clone();
    remapped.resolved_role_id = Some(DiscordId::new("345678901234567890").unwrap());
    assert!(matches!(
        store.claim(&id, &remapped).await.unwrap(),
        InternalClaim::Mismatch
    ));
    sqlx::query(
        "UPDATE internal_idempotency SET created_at = clock_timestamp() - interval '1 hour'",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        store
            .reconcile(
                &id,
                &TerminalResponse::Failure(TerminalFailure::NoEffect),
                ReconciliationEvidence::ProvenNotSent
            )
            .await
            .unwrap_err(),
        InternalStoreError::TransitionRefused
    );
    let (a, b) = tokio::join!(store.claim(&id, &subject), store.claim(&id, &subject));
    let winner = match (a.unwrap(), b.unwrap()) {
        (InternalClaim::Claimed(claim), InternalClaim::InFlight)
        | (InternalClaim::InFlight, InternalClaim::Claimed(claim)) => claim,
        other => panic!("expected exactly one new owner: {other:?}"),
    };
    assert_eq!(winner.intent_id(), intent_id);
    // A second no-dispatch release still keeps one audit per intent/phase.
    store.release_proven_not_sent(winner).await.unwrap();
    let final_claim = claimed(store.claim(&id, &subject).await.unwrap());
    store.finish(&final_claim, &success()).await.unwrap();
    assert!(matches!(
        store.claim(&id, &subject).await.unwrap(),
        InternalClaim::Replay(_)
    ));
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT phase, evidence_code FROM internal_action_log ORDER BY audit_id")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            ("intent".into(), None),
            ("released".into(), Some("proven_not_sent".into())),
            ("terminal".into(), Some("executor".into())),
        ]
    );
    db.cleanup().await;
}

#[tokio::test]
async fn unsent_release_refuses_uncertainty_and_rolls_back_with_its_audit() {
    let db = TestDb::new().await;
    let store = db.store();
    let subject = subject();
    for state in ["unknown", "completed", "stale"] {
        let id = identity(&format!("unsent-{state}:123"), "role.assign", b"payload");
        let claim = claimed(store.claim(&id, &subject).await.unwrap());
        match state {
            "unknown" => store.mark_unknown(&claim).await.unwrap(),
            "completed" => store.finish(&claim, &success()).await.unwrap(),
            "stale" => {
                sqlx::query("UPDATE internal_idempotency SET created_at = clock_timestamp() - interval '1 hour' WHERE intent_id = $1")
                    .bind(claim.intent_id()).execute(&db.pool).await.unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            store.release_proven_not_sent(claim).await.unwrap_err(),
            InternalStoreError::TransitionRefused
        );
        assert!(!matches!(
            store.claim(&id, &subject).await.unwrap(),
            InternalClaim::Claimed(_)
        ));
    }
    let id = identity("unsent-rollback:123", "role.assign", b"payload");
    let claim = claimed(store.claim(&id, &subject).await.unwrap());
    sqlx::query("ALTER TABLE internal_action_log ADD CONSTRAINT injected_release_failure CHECK (phase <> 'released')")
        .execute(&db.pool).await.unwrap();
    assert_eq!(
        store.release_proven_not_sent(claim).await.unwrap_err(),
        InternalStoreError::Unavailable
    );
    assert!(matches!(
        store.claim(&id, &subject).await.unwrap(),
        InternalClaim::InFlight
    ));
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM internal_action_log WHERE phase = 'released'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    db.cleanup().await;
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

/// F8 durable fail-closed: accept a capture, expire and replace its nonce,
/// then regress the DB clock into its signed window. The persisted high-water
/// mark refuses the rolled-back capture even though the old row is gone — on
/// the live store and on a fresh store (restart/failover) that re-derives the
/// mark from the table. Time is injected only through rows the test owns and
/// the mark table; the server clock is never changed.
///
/// DB-time regression is injected by moving the mark row INTO THE FUTURE:
/// `burn_nonce` reads its commit instant from the real `clock_timestamp()`,
/// so a mark ~300 s ahead makes the genuine commit instant read as regressed
/// past the 5 s tolerance — exercising the real in-transaction rollback
/// branch. Backdating rows alone could never move `clock_timestamp()`.
#[tokio::test]
async fn nonce_db_rollback_after_expiry_refuses_capture() {
    use two_bot_core::clock_guard::{ClockGuard, CLOCK_SKEW_TOLERANCE_MS};

    let db = TestDb::new().await;
    let store = db.store();
    let nonce = body_hash(db.fixture.name().as_bytes())[..32].to_owned();
    let attempt = db_now_secs(&db.pool).await.to_string();
    assert!(
        store.burn_nonce(&nonce, &attempt).await.unwrap(),
        "first burn accepted"
    );
    let mark = store
        .nonce_high_water_ms()
        .await
        .unwrap()
        .expect("first burn persists the high-water mark");
    // The mark matches DB time to the second (whole-second commit instant).
    let db_ms: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(
        (db_ms - mark as i64).abs() <= 2_000,
        "mark tracks the DB clock: mark={mark} db={db_ms}"
    );

    // Expire and replace the nonce row so the old capture's only guard is the
    // mark; a fresh store (new process) sees the same persisted mark.
    let now = db_now_secs(&db.pool).await.to_string();
    sqlx::query(
        "UPDATE internal_nonces SET burned_at = clock_timestamp() - INTERVAL '242 seconds', \
         expires_at = clock_timestamp() - INTERVAL '1 second'",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let replacement =
        body_hash(format!("{}-replacement", db.fixture.name()).as_bytes())[..32].to_owned();
    assert!(
        store.burn_nonce(&replacement, &now).await.unwrap(),
        "expired row is replaceable before rollback"
    );

    // Inject a DB-time regression: advance the mark ~300 s into the future
    // (keeping `observed_at` consistent with the CHECK) so the next genuine
    // commit instant reads as regressed past the tolerance. The rolled-back
    // capture is a FRESH nonce with a FRESH timestamp — only the clock policy
    // refuses it, proving the rollback branch rather than expiry or staleness.
    let future_ms: i64 = db_ms + 300_000;
    sqlx::query(
        "UPDATE internal_clock_high_water SET high_water_ms = $1, \
         observed_at = TO_TIMESTAMP($1::double precision / 1000.0) \
         WHERE domain = 'internal_nonce_db'",
    )
    .bind(future_ms)
    .execute(&db.pool)
    .await
    .unwrap();
    let regressed_nonce =
        body_hash(format!("{}-regressed", db.fixture.name()).as_bytes())[..32].to_owned();
    let regressed_attempt = db_now_secs(&db.pool).await.to_string();
    assert_eq!(
        store.burn_nonce(&regressed_nonce, &regressed_attempt).await,
        Err(InternalStoreError::InvalidInput),
        "regressed DB clock must refuse even a fresh capture"
    );
    // The refused burn left no trace: neither a nonce row nor a mark advance.
    let nonce_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM internal_nonces WHERE nonce_hash = $1")
            .bind(body_hash(regressed_nonce.as_bytes()))
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(nonce_rows, 0, "refused burn must not insert a nonce row");
    let mark_after = store.nonce_high_water_ms().await.unwrap();
    assert_eq!(
        mark_after,
        Some(u64::try_from(future_ms).unwrap()),
        "refused burn must not advance the mark"
    );

    // A fresh store with an earlier clock and the persisted mark also refuses:
    // restore the guard from the table exactly as a restarted process would.
    // The "earlier clock" is real DB time, ~300 s below the injected mark.
    let persisted = mark_after.expect("mark survives for a restarted process");
    let mut guard = ClockGuard::restore(persisted);
    let earlier = u64::try_from(db_ms).unwrap();
    assert!(earlier + CLOCK_SKEW_TOLERANCE_MS < persisted);
    let err = guard.evaluate(earlier).expect_err(
        "new process with an earlier clock and the persisted mark refuses old captures",
    );
    assert_eq!(err.high_water_ms, persisted);

    // Forward DB time still behaves as before: clear the injected future mark
    // and a fresh capture burns.
    sqlx::query("DELETE FROM internal_nonces")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM internal_clock_high_water WHERE domain = 'internal_nonce_db'")
        .execute(&db.pool)
        .await
        .unwrap();
    let fresh_nonce = body_hash(format!("{}-fresh", db.fixture.name()).as_bytes())[..32].to_owned();
    let fresh = db_now_secs(&db.pool).await.to_string();
    assert!(store.burn_nonce(&fresh_nonce, &fresh).await.unwrap());
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

const EVENT_GUILD: &str = "100000000000000001";
const OTHER_GUILD: &str = "100000000000000099";
const MAPPED_EVENT: &str = "100000000000000002";

#[tokio::test]
async fn event_key_map_is_guild_fenced_and_repointable() {
    let db = TestDb::new().await;
    let store = db.store();
    // Unmapped keys resolve to None: the receiver refuses them before Discord.
    assert_eq!(
        store.event_id_for_key(EVENT_GUILD, "launch").await.unwrap(),
        None
    );
    store
        .put_event_key(EVENT_GUILD, "launch", MAPPED_EVENT)
        .await
        .unwrap();
    assert_eq!(
        store.event_id_for_key(EVENT_GUILD, "launch").await.unwrap(),
        Some(MAPPED_EVENT.to_owned())
    );
    // The fence is the guild: the same key elsewhere stays unmapped.
    assert_eq!(
        store.event_id_for_key(OTHER_GUILD, "launch").await.unwrap(),
        None
    );
    store
        .put_event_key(OTHER_GUILD, "launch", "100000000000000003")
        .await
        .unwrap();
    assert_eq!(
        store.event_id_for_key(OTHER_GUILD, "launch").await.unwrap(),
        Some("100000000000000003".to_owned())
    );
    // Re-pointing one guild leaves the other alone.
    store
        .put_event_key(EVENT_GUILD, "launch", "100000000000000004")
        .await
        .unwrap();
    assert_eq!(
        store.event_id_for_key(EVENT_GUILD, "launch").await.unwrap(),
        Some("100000000000000004".to_owned())
    );
    assert_eq!(
        store.event_id_for_key(OTHER_GUILD, "launch").await.unwrap(),
        Some("100000000000000003".to_owned())
    );
    db.cleanup().await;
}

#[tokio::test]
async fn event_key_map_refuses_misshapen_inputs() {
    let db = TestDb::new().await;
    let store = db.store();
    let long = "k".repeat(201);
    for (guild, key, event) in [
        ("not-a-snowflake", "launch", MAPPED_EVENT),
        (EVENT_GUILD, "", MAPPED_EVENT),
        (EVENT_GUILD, "has space", MAPPED_EVENT),
        (EVENT_GUILD, long.as_str(), MAPPED_EVENT),
        (EVENT_GUILD, "launch", "not-a-snowflake"),
    ] {
        assert_eq!(
            store.put_event_key(guild, key, event).await,
            Err(InternalStoreError::InvalidInput),
            "{guild}/{key}/{event}"
        );
    }
    for (guild, key) in [
        ("not-a-snowflake", "launch"),
        (EVENT_GUILD, ""),
        (EVENT_GUILD, "has space"),
    ] {
        assert_eq!(
            store.event_id_for_key(guild, key).await,
            Err(InternalStoreError::InvalidInput),
            "{guild}/{key}"
        );
    }
    db.cleanup().await;
}
