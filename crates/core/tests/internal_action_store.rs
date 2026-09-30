//! Explicitly requested tests require the authorized test container; never skip
//! configured failures or consult a production/staging URL. Each test owns a schema.
#![cfg(feature = "db")]

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use two_bot_core::internal_action_store::{
    AuditSubject, DiscordId, ExecutionClaim, InternalActionStore, InternalClaim,
    InternalStoreError, ReconciliationEvidence, RequestIdentity, TerminalFailure, TerminalResponse,
};
use two_bot_core::{body_hash, CLAIM_STALE_SECONDS, NONCE_TTL_SECONDS, SKEW_SECONDS};

fn test_options(url: &str) -> Result<PgConnectOptions, &'static str> {
    if url.contains(['?', '#']) {
        return Err("no test URL overrides");
    }
    if !(url.starts_with("postgres://agent_test:@") || url.starts_with("postgresql://agent_test:@"))
    {
        return Err("explicit agent_test empty password required");
    }
    let options = PgConnectOptions::from_str(url).map_err(|_| "invalid test URL")?;
    if options.get_host() != "agent-testdb"
        || options.get_port() != 5432
        || options.get_socket().is_some()
        || options.get_username() != "agent_test"
        || options.get_options().is_some()
        || options.get_database() != Some("agent_test")
    {
        return Err("test-container target only");
    }
    Ok(options)
}

fn schema_name() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "ia10606_{}_{now}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn ddl(sql: &str, schema: &str) -> sqlx::AssertSqlSafe<String> {
    assert!(schema.starts_with("ia10606_") && schema.len() <= 63);
    assert!(schema
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    // Only a generated, strictly guarded owned schema interpolates into DDL.
    sqlx::AssertSqlSafe(format!("{sql} {schema}"))
}

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    options: PgConnectOptions,
    schema: String,
}

impl TestDb {
    async fn new() -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("explicit DB test requires TWO_TEST_DATABASE_URL (agent-testdb only)");
        let options = test_options(&url).expect("refusing non-test-container target");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .unwrap();
        let schema = schema_name();
        sqlx::query(ddl("CREATE SCHEMA", &schema))
            .execute(&admin)
            .await
            .unwrap();
        let pool = Self::connect(&options, &schema).await;
        sqlx::raw_sql(include_str!(
            "../../cutover/migrations/0340_internal_actions.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        Self {
            admin,
            pool,
            options,
            schema,
        }
    }

    async fn connect(options: &PgConnectOptions, schema: &str) -> PgPool {
        PgPoolOptions::new()
            .max_connections(6)
            .connect_with(options.clone().options([("search_path", schema)]))
            .await
            .unwrap()
    }

    async fn independent_pool(&self) -> PgPool {
        Self::connect(&self.options, &self.schema).await
    }

    fn store(&self) -> InternalActionStore {
        InternalActionStore::new(self.pool.clone())
    }

    async fn cleanup(self) {
        self.pool.close().await;
        let drop_schema = ddl("DROP SCHEMA", &self.schema);
        sqlx::query(sqlx::AssertSqlSafe(format!("{} CASCADE", drop_schema.0)))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
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

#[test]
fn test_guard_rejects_redirects_credentials_and_non_test_targets() {
    assert!(test_options("postgres://agent_test:@agent-testdb:5432/agent_test").is_ok());
    for url in [
        "postgres://agent_test@agent-testdb:5432/agent_test",
        "postgres://agent_test:other@agent-testdb:5432/agent_test",
        "postgres://agent_test:@staging:5432/agent_test",
        "postgres://agent_test:@agent-testdb:5433/agent_test",
        "postgres://other:@agent-testdb:5432/agent_test",
        "postgres://agent_test:@agent-testdb:5432/production",
        "postgres://agent_test:@agent-testdb:5432/agent_test?hostaddr=127.0.0.1",
        "postgres://agent_test:@agent-testdb:5432/agent_test#fragment",
    ] {
        assert!(test_options(url).is_err());
    }
}

#[tokio::test]
#[ignore = "requires agent-testdb; CI explicitly runs this suite"]
async fn nonce_race_restart_and_expiry_window() {
    let db = TestDb::new().await;
    let second = db.independent_pool().await;
    let a = db.store();
    let b = InternalActionStore::new(second.clone());
    let nonce = body_hash(schema_name().as_bytes())[..32].to_owned();
    let mut tasks = Vec::new();
    for i in 0..24 {
        let store = if i % 2 == 0 { a.clone() } else { b.clone() };
        let nonce = nonce.clone();
        tasks.push(tokio::spawn(async move {
            store.burn_nonce(&nonce).await.unwrap()
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
    sqlx::query("UPDATE internal_nonces SET burned_at = clock_timestamp() - INTERVAL '240 seconds', expires_at = clock_timestamp() + INTERVAL '1 second'")
        .execute(&db.pool).await.unwrap();
    assert!(!b.burn_nonce(&nonce).await.unwrap());
    sqlx::query("UPDATE internal_nonces SET burned_at = clock_timestamp() - INTERVAL '242 seconds', expires_at = clock_timestamp() - INTERVAL '1 second'")
        .execute(&db.pool).await.unwrap();
    assert!(b.burn_nonce(&nonce).await.unwrap());
    second.close().await;
    let restarted = db.independent_pool().await;
    assert!(!InternalActionStore::new(restarted.clone())
        .burn_nonce(&nonce)
        .await
        .unwrap());
    assert_eq!(
        a.burn_nonce("not-a-valid-nonce").await,
        Err(InternalStoreError::InvalidInput)
    );
    restarted.close().await;
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb; CI explicitly runs this suite"]
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
#[ignore = "requires agent-testdb; CI explicitly runs this suite"]
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
#[ignore = "requires agent-testdb; CI explicitly runs this suite"]
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
    db.pool.close().await;
    assert!(matches!(
        store.claim(&id, &subject()).await,
        Err(InternalStoreError::Unavailable)
    ));
    let nonce = body_hash(schema_name().as_bytes())[..32].to_owned();
    assert_eq!(
        store.burn_nonce(&nonce).await,
        Err(InternalStoreError::Unavailable)
    );
    assert_eq!(
        store.claim_discord_event("gateway:session:123").await,
        Err(InternalStoreError::Unavailable)
    );
    db.cleanup().await;
}

#[tokio::test]
#[ignore = "requires agent-testdb; CI explicitly runs this suite"]
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
