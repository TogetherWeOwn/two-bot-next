//! Explicit opt-in, isolated-schema integration test. No inherited DATABASE_URL
//! or credentials: only the agent-testdb container is allowed here.

use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use two_bot_core::self_roles::event_order_for_event_id;
use two_bot_core::{PanelMode, RoleOperation, SettledOutcome};
use two_bot_cutover::self_role_store::{
    AuditEffects, PanelClaim, PanelClaimResult, PanelKey, SelfRoleAudit, SelfRoleStore, StoreError,
};

const TEST_NOW_MS: i64 = 1_700_000_000_000;
const TEST_POOL_SIZE: u32 = 5;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn row(id: &str, order: &str) -> SelfRoleAudit {
    SelfRoleAudit {
        event_id: id.to_owned(),
        event_order: Some(order.to_owned()),
        guild_id: "test-guild".to_owned(),
        member_id: "test-member".to_owned(),
        panel_id: "games".to_owned(),
        source_id: "test-message".to_owned(),
        option_key: Some("chess".to_owned()),
        role_id: Some("role-a".to_owned()),
        source: PanelMode::Button,
        operation: RoleOperation::Replace,
        outcome: SettledOutcome::Switched,
        code: None,
        reason: None,
        effects: AuditEffects::default(),
        desired_role_ids: vec!["role-a".to_owned()],
        pre_mutation_role_ids: vec!["role-b".to_owned()],
    }
}

fn acquired(result: PanelClaimResult) -> PanelClaim {
    match result {
        PanelClaimResult::Acquired(claim) => claim,
        other => panic!("expected acquired, got {other:?}"),
    }
}

fn panel_key(audit: &SelfRoleAudit) -> PanelKey {
    PanelKey {
        guild_id: audit.guild_id.clone(),
        member_id: audit.member_id.clone(),
        panel_id: audit.panel_id.clone(),
    }
}

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn leases_recovery_ordering_and_atomic_settlement() -> TestResult {
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
    // Generated identifier contains digits/underscore only. Each run gets its
    // own namespace, and cleanup never touches another test's tables.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let schema = format!("self_roles_{}_{}", std::process::id(), nonce);
    // Audited: identifier is a fixed prefix plus numeric PID and timestamp.
    // https://docs.rs/sqlx/0.9.0/sqlx/struct.AssertSqlSafe.html
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await?;
    let search_path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(TEST_POOL_SIZE)
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
    let result = exercise(&pool).await;
    pool.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await?;
    admin.close().await;
    result
}

async fn pending_intent_initialization(pool: &PgPool) -> TestResult {
    let clock = Arc::new(AtomicI64::new(TEST_NOW_MS));
    let store = SelfRoleStore::with_test_clock(pool.clone(), 300, clock.clone())?;
    let mut pending = row("pending-intent", "pending-order");
    pending.desired_role_ids.clear();
    pending.pre_mutation_role_ids.clear();
    let original = store.claim_pending_audit(&pending).await?.unwrap();
    assert!(!original.intent_initialized);
    // Admission can crash before any member fetch. Recovery must not treat
    // placeholder [] as a committed empty target.
    clock.store(TEST_NOW_MS + 300, Ordering::SeqCst);
    let mut expired = original.clone();
    assert!(!store.initialize_intent(&mut expired, &[], &[]).await?);
    let mut recovered = store.claim_pending_audit(&pending).await?.unwrap();
    assert!(recovered.recovered);
    assert!(!recovered.intent_initialized);
    assert!(!store.initialize_intent(&mut expired, &[], &[]).await?);
    // Empty is a real initialized intent, not a signal to replan on retry.
    assert!(store.initialize_intent(&mut recovered, &[], &[]).await?);
    assert!(recovered.intent_initialized);
    assert!(
        !store
            .initialize_intent(&mut recovered, &["role-new".into()], &[])
            .await?
    );
    clock.store(TEST_NOW_MS + 600, Ordering::SeqCst);
    let mut mismatched = pending.clone();
    mismatched.member_id = "another-member".into();
    assert!(store.claim_pending_audit(&mismatched).await?.is_none());
    mismatched = pending.clone();
    mismatched.event_order = Some("different-order".into());
    assert!(store.claim_pending_audit(&mismatched).await?.is_none());
    let recovered = store.claim_pending_audit(&pending).await?.unwrap();
    assert!(recovered.intent_initialized);
    assert!(recovered.desired_role_ids.is_empty());
    assert!(recovered.pre_mutation_role_ids.is_empty());
    let mut rejected = pending;
    rejected.outcome = SettledOutcome::Rejected;
    store.finish_audit(&rejected, &recovered).await?;
    Ok(())
}

async fn exercise(pool: &PgPool) -> TestResult {
    let migration = include_str!("../migrations/0200_self_roles.sql");
    sqlx::raw_sql(migration).execute(pool).await?;
    // Migration replay is harmless and does not lose state.
    sqlx::raw_sql(migration).execute(pool).await?;
    let (audit_columns,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM information_schema.columns WHERE table_schema=current_schema() AND table_name='self_role_audit'",
    ).fetch_one(pool).await?;
    let (panel_columns,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM information_schema.columns WHERE table_schema=current_schema() AND table_name='self_role_panel_claims'",
    ).fetch_one(pool).await?;
    assert_eq!((audit_columns, panel_columns), (27, 10));
    sqlx::raw_sql(include_str!(
        "../migrations/0201_self_role_intent_initialization.sql"
    ))
    .execute(pool)
    .await?;
    pending_intent_initialization(pool).await?;

    let clock = Arc::new(AtomicI64::new(TEST_NOW_MS));
    let store = SelfRoleStore::with_test_clock(pool.clone(), 300, clock.clone())?;
    assert!(matches!(
        SelfRoleStore::with_lease(pool.clone(), 0),
        Err(StoreError::InvalidLease)
    ));
    let audit = row("event-one", "0001");
    // Distinct connections concurrently arbitrate the same event id.
    let (a, b) = tokio::join!(store.claim_audit(&audit), store.claim_audit(&audit));
    let (a, b) = (a?, b?);
    assert_eq!(usize::from(a.is_some()) + usize::from(b.is_some()), 1);
    let original = a.or(b).expect("single winner");
    assert_eq!(original.generation, 1);
    assert_eq!(original.renew_after_ms, 100);
    assert!(store.owns_claim(&original).await?);
    clock.store(TEST_NOW_MS + 200, Ordering::SeqCst);
    assert!(store.renew_claim(&original).await?);
    clock.store(TEST_NOW_MS + 300, Ordering::SeqCst);
    assert!(store.claim_audit(&audit).await?.is_none());
    // Strict expiry boundary: expired claims cannot renew or authorize work.
    clock.store(TEST_NOW_MS + 500, Ordering::SeqCst);
    assert!(!store.owns_claim(&original).await?);
    assert!(!store.renew_claim(&original).await?);
    let mut conflicting_retry = audit.clone();
    conflicting_retry.desired_role_ids = vec!["role-z".to_owned()];
    conflicting_retry.pre_mutation_role_ids.clear();
    let recovered = store
        .claim_audit(&conflicting_retry)
        .await?
        .expect("recovery");
    assert!(recovered.recovered);
    assert_eq!(recovered.generation, 2);
    assert_ne!(recovered.token, original.token);
    assert_eq!(recovered.desired_role_ids, audit.desired_role_ids);
    assert_eq!(recovered.pre_mutation_role_ids, audit.pre_mutation_role_ids);
    assert!(!store.renew_claim(&original).await?);
    assert!(
        !store
            .update_audit_effects(&original, &AuditEffects::default())
            .await?
    );
    assert!(matches!(
        store.finish_audit(&audit, &original).await,
        Err(StoreError::StaleClaim)
    ));
    store.finish_audit(&audit, &recovered).await?;
    clock.store(TEST_NOW_MS + 1_500, Ordering::SeqCst);
    assert!(store.claim_audit(&audit).await?.is_none());
    let (outcome, desired, before, expires): (String, String, String, Option<OffsetDateTime>) = sqlx::query_as(
        "SELECT outcome,desired_role_ids,pre_mutation_role_ids,processing_expires_at FROM self_role_audit WHERE event_id='event-one'",
    ).fetch_one(pool).await?;
    assert_eq!(outcome, "switched");
    assert_eq!(desired, "[\"role-a\"]");
    assert_eq!(before, "[\"role-b\"]");
    assert!(expires.is_none());

    clock.store(TEST_NOW_MS, Ordering::SeqCst);
    let key = panel_key(&audit);
    let old = row("event-old", "0002");
    let old_event = store.claim_audit(&old).await?.expect("old event");
    let mut first = acquired(store.claim_panel(&key, Some(("event-old", "0002"))).await?);
    assert!(!first.target.committed);
    assert!(store.set_panel_claim_option(&mut first, Some("go")).await?);
    assert!(first.target.committed);
    let newer = row("event-new", "0003");
    let newer_event = store.claim_audit(&newer).await?.expect("new event");
    assert!(matches!(
        store.claim_panel(&key, Some(("event-new", "0003"))).await?,
        PanelClaimResult::Busy
    ));
    // A busy contender is not allowed to reject the current worker's audit.
    assert!(store.owns_claim(&old_event).await?);
    clock.store(TEST_NOW_MS + 200, Ordering::SeqCst);
    assert!(store.renew_panel_claim(&first).await?);
    clock.store(TEST_NOW_MS + 300, Ordering::SeqCst);
    assert!(store.owns_panel_claim(&first).await?);
    clock.store(TEST_NOW_MS + 500, Ordering::SeqCst);
    assert!(!store.renew_panel_claim(&first).await?);
    let mut second = acquired(store.claim_panel(&key, Some(("event-new", "0003"))).await?);
    assert_eq!(second.generation, 2);
    assert_eq!(second.target.option_key.as_deref(), Some("go"));
    assert!(second.target.committed); // uncommitted incoming intent never overwrites go
    assert!(!store.release_panel_claim(&first).await?);
    assert!(
        !store
            .set_panel_claim_option(&mut first, Some("chess"))
            .await?
    );
    assert!(!store.renew_claim(&old_event).await?);
    let (rejected, code): (String, String) =
        sqlx::query_as("SELECT outcome,code FROM self_role_audit WHERE event_id='event-old'")
            .fetch_one(pool)
            .await?;
    assert_eq!(
        (rejected.as_str(), code.as_str()),
        ("rejected", "superseded_by_later_event")
    );
    assert!(matches!(
        store
            .claim_panel(&key, Some(("event-stale", "0001")))
            .await?,
        PanelClaimResult::Superseded(_)
    ));

    // A stale event must roll back the panel target publication as well.
    assert!(matches!(
        store
            .finish_audit_and_set_panel_option(&old, &old_event, &mut second, Some("chess"))
            .await,
        Err(StoreError::StaleClaim)
    ));
    let (target,): (Option<String>,) = sqlx::query_as(
        "SELECT latest_option_key FROM self_role_panel_claims WHERE guild_id='test-guild' AND member_id='test-member' AND panel_id='games'",
    ).fetch_one(pool).await?;
    assert_eq!(target.as_deref(), Some("go"));
    // Recover the current event first; publishing requires its exact fence.
    let newer_recovered = store
        .claim_audit(&newer)
        .await?
        .expect("recover current event");
    assert!(!store.owns_claim(&newer_event).await?);
    let mut settled = newer.clone();
    settled.effects.added_role_ids = vec!["role-a".to_owned()];
    settled.effects.removed_role_ids = vec!["role-b".to_owned()];
    assert!(
        store
            .finish_audit_and_set_panel_option(
                &settled,
                &newer_recovered,
                &mut second,
                Some("chess")
            )
            .await?
    );
    assert_eq!(second.target.option_key.as_deref(), Some("chess"));
    assert!(store.release_panel_claim(&second).await?);
    // A maintenance claim retains chronology and the last committed target.
    let mut repair = acquired(store.claim_panel(&key, None).await?);
    assert_eq!(repair.target.latest_event_order.as_deref(), Some("0003"));
    assert_eq!(repair.target.option_key.as_deref(), Some("chess"));
    assert!(store.set_panel_claim_option(&mut repair, None).await?);
    assert!(repair.target.committed && repair.target.option_key.is_none());
    assert!(store.release_panel_claim(&repair).await?);
    let empty = acquired(store.claim_panel(&key, None).await?);
    assert!(empty.target.committed && empty.target.option_key.is_none());

    // Independent panels do not contend; same lane has one winner even when
    // the incoming event ids differ.
    let other = PanelKey {
        panel_id: "other".to_owned(),
        ..key.clone()
    };
    let (a, b) = tokio::join!(
        store.claim_panel(&other, Some(("a", "0010"))),
        store.claim_panel(&other, Some(("b", "0010")))
    );
    assert_eq!(
        usize::from(matches!(a?, PanelClaimResult::Acquired(_)))
            + usize::from(matches!(b?, PanelClaimResult::Acquired(_))),
        1
    );
    assert!(store.owns_panel_claim(&empty).await?);

    exercise_effect_recovery(pool).await?;
    exercise_superseded_evidence(pool).await?;
    exercise_generated_event_order(pool).await?;
    exercise_db_clock_waits(pool).await?;
    exercise_atomic_finish_wait(pool).await?;
    Ok(())
}

async fn stored_effects(pool: &PgPool, event_id: &str) -> TestResult<AuditEffects> {
    let (fields,): (Vec<String>,) = sqlx::query_as(
        "SELECT ARRAY[added_role_ids,removed_role_ids,
         attempted_added_role_ids,attempted_removed_role_ids,
         compensated_added_role_ids,compensated_removed_role_ids,
         unresolved_added_role_ids,unresolved_removed_role_ids]
         FROM self_role_audit WHERE event_id=$1",
    )
    .bind(event_id)
    .fetch_one(pool)
    .await?;
    Ok(AuditEffects {
        added_role_ids: serde_json::from_str(&fields[0])?,
        removed_role_ids: serde_json::from_str(&fields[1])?,
        attempted_added_role_ids: serde_json::from_str(&fields[2])?,
        attempted_removed_role_ids: serde_json::from_str(&fields[3])?,
        compensated_added_role_ids: serde_json::from_str(&fields[4])?,
        compensated_removed_role_ids: serde_json::from_str(&fields[5])?,
        unresolved_added_role_ids: serde_json::from_str(&fields[6])?,
        unresolved_removed_role_ids: serde_json::from_str(&fields[7])?,
    })
}

fn assert_effects(actual: &AuditEffects, expected: &AuditEffects) {
    // Historical unions are sets; do not depend on their serialized ordering.
    for (actual, expected) in [
        (&actual.added_role_ids, &expected.added_role_ids),
        (&actual.removed_role_ids, &expected.removed_role_ids),
        (
            &actual.attempted_added_role_ids,
            &expected.attempted_added_role_ids,
        ),
        (
            &actual.attempted_removed_role_ids,
            &expected.attempted_removed_role_ids,
        ),
        (
            &actual.compensated_added_role_ids,
            &expected.compensated_added_role_ids,
        ),
        (
            &actual.compensated_removed_role_ids,
            &expected.compensated_removed_role_ids,
        ),
        (
            &actual.unresolved_added_role_ids,
            &expected.unresolved_added_role_ids,
        ),
        (
            &actual.unresolved_removed_role_ids,
            &expected.unresolved_removed_role_ids,
        ),
    ] {
        let mut actual = actual.clone();
        let mut expected = expected.clone();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
    }
}

async fn exercise_effect_recovery(pool: &PgPool) -> TestResult {
    let clock = Arc::new(AtomicI64::new(TEST_NOW_MS));
    let store = SelfRoleStore::with_test_clock(pool.clone(), 300, clock.clone())?;
    let mut audit = row("effects-event", "effects-order");
    audit.panel_id = "effects".to_owned();
    let original = store
        .claim_audit(&audit)
        .await?
        .expect("original effects claim");
    let checkpoint = AuditEffects {
        added_role_ids: vec!["observed-add-one".to_owned()],
        removed_role_ids: vec!["observed-remove-one".to_owned()],
        attempted_added_role_ids: vec!["attempt-add-one".to_owned()],
        attempted_removed_role_ids: vec!["attempt-remove-one".to_owned()],
        compensated_added_role_ids: vec!["compensate-add-one".to_owned()],
        compensated_removed_role_ids: vec!["compensate-remove-one".to_owned()],
        unresolved_added_role_ids: vec!["unresolved-add-one".to_owned()],
        unresolved_removed_role_ids: vec!["unresolved-remove-one".to_owned()],
    };
    assert!(store.update_audit_effects(&original, &checkpoint).await?);
    assert_eq!(stored_effects(pool, &audit.event_id).await?, checkpoint);
    clock.store(TEST_NOW_MS + 300, Ordering::SeqCst);
    let recovered = store
        .claim_audit(&audit)
        .await?
        .expect("recover effects claim");
    assert!(recovered.recovered);
    assert_eq!(recovered.effects, checkpoint); // all eight arrays survive recovery
    assert!(
        !store
            .update_audit_effects(&original, &AuditEffects::default())
            .await?
    );
    let next = AuditEffects {
        added_role_ids: vec!["observed-add-two".to_owned()],
        removed_role_ids: vec!["observed-remove-two".to_owned()],
        attempted_added_role_ids: vec!["attempt-add-two".to_owned(), "attempt-add-one".to_owned()],
        attempted_removed_role_ids: vec!["attempt-remove-two".to_owned()],
        compensated_added_role_ids: vec!["compensate-add-two".to_owned()],
        compensated_removed_role_ids: vec![
            "compensate-remove-one".to_owned(),
            "compensate-remove-two".to_owned(),
        ],
        unresolved_added_role_ids: vec!["unresolved-add-two".to_owned()],
        unresolved_removed_role_ids: vec!["unresolved-remove-two".to_owned()],
    };
    assert!(store.update_audit_effects(&recovered, &next).await?);
    let cumulative = AuditEffects {
        attempted_added_role_ids: vec!["attempt-add-one".to_owned(), "attempt-add-two".to_owned()],
        attempted_removed_role_ids: vec![
            "attempt-remove-one".to_owned(),
            "attempt-remove-two".to_owned(),
        ],
        compensated_added_role_ids: vec![
            "compensate-add-one".to_owned(),
            "compensate-add-two".to_owned(),
        ],
        compensated_removed_role_ids: vec![
            "compensate-remove-one".to_owned(),
            "compensate-remove-two".to_owned(),
        ],
        ..next
    };
    assert_effects(&stored_effects(pool, &audit.event_id).await?, &cumulative);
    // A fresh empty final snapshot clears observed/ambiguous effects, not history.
    store.finish_audit(&audit, &recovered).await?;
    let final_effects = AuditEffects {
        added_role_ids: vec![],
        removed_role_ids: vec![],
        unresolved_added_role_ids: vec![],
        unresolved_removed_role_ids: vec![],
        ..cumulative
    };
    assert_effects(
        &stored_effects(pool, &audit.event_id).await?,
        &final_effects,
    );
    assert!(store.claim_audit(&audit).await?.is_none());
    Ok(())
}

async fn exercise_superseded_evidence(pool: &PgPool) -> TestResult {
    let clock = Arc::new(AtomicI64::new(TEST_NOW_MS));
    let store = SelfRoleStore::with_test_clock(pool.clone(), 300, clock.clone())?;
    let mut old = row("superseded-evidence-old", "sup-evidence-0001");
    old.panel_id = "superseded-evidence".to_owned();
    let mut newer = row("superseded-evidence-new", "sup-evidence-0002");
    newer.panel_id = old.panel_id.clone();
    let old_event = store.claim_audit(&old).await?.expect("superseded event");
    let key = panel_key(&old);
    acquired(
        store
            .claim_panel(
                &key,
                Some((&old.event_id, old.event_order.as_deref().unwrap())),
            )
            .await?,
    );
    // Attempt history recorded before supersession survives.
    let pre = AuditEffects {
        attempted_added_role_ids: vec!["role-a".to_owned()],
        ..Default::default()
    };
    assert!(store.update_audit_effects(&old_event, &pre).await?);
    // An event-generation transfer rejects the former worker's evidence.
    clock.store(TEST_NOW_MS + 500, Ordering::SeqCst);
    let current = store
        .claim_audit(&old)
        .await?
        .expect("recover superseded event");
    assert!(current.recovered);
    assert!(!store.record_superseded_effects(&old_event, &pre).await?);
    assert!(
        !store
            .update_audit_effects(&old_event, &AuditEffects::default())
            .await?
    );
    // A newer lane admission supersedes the transferred generation.
    store.claim_audit(&newer).await?.expect("newer event");
    let mut second = acquired(
        store
            .claim_panel(
                &key,
                Some((&newer.event_id, newer.event_order.as_deref().unwrap())),
            )
            .await?,
    );
    let (outcome, code, token, generation): (String, Option<String>, Option<String>, i32) = sqlx::query_as(
        "SELECT outcome,code,claim_token,claim_generation FROM self_role_audit WHERE event_id='superseded-evidence-old'",
    ).fetch_one(pool).await?;
    assert_eq!(outcome, "rejected");
    assert_eq!(code.as_deref(), Some("superseded_by_later_event"));
    assert_eq!(token.as_deref(), Some(current.token.expose().as_str()));
    assert_eq!(generation, current.generation);
    // Late result/compensation evidence records under the still-current
    // token/generation; the rejection stays terminal and authorizes neither
    // settlement nor panel-target publication.
    let late = AuditEffects {
        added_role_ids: vec!["role-a".to_owned()],
        compensated_added_role_ids: vec!["role-b".to_owned()],
        unresolved_removed_role_ids: vec!["role-c".to_owned()],
        ..Default::default()
    };
    assert!(store.record_superseded_effects(&current, &late).await?);
    let (outcome, code): (String, Option<String>) = sqlx::query_as(
        "SELECT outcome,code FROM self_role_audit WHERE event_id='superseded-evidence-old'",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(outcome.as_str(), "rejected");
    assert_eq!(code.as_deref(), Some("superseded_by_later_event"));
    let mut expected = late.clone();
    expected.attempted_added_role_ids = vec!["role-a".to_owned()];
    assert_effects(&stored_effects(pool, &old.event_id).await?, &expected);
    assert!(
        !store
            .update_audit_effects(&current, &AuditEffects::default())
            .await?
    );
    assert!(matches!(
        store.finish_audit(&old, &current).await,
        Err(StoreError::StaleClaim)
    ));
    assert!(matches!(
        store
            .finish_audit_and_set_panel_option(&old, &current, &mut second, Some("chess"))
            .await,
        Err(StoreError::StaleClaim)
    ));
    assert!(second.target.option_key.is_none());
    Ok(())
}

async fn exercise_generated_event_order(pool: &PgPool) -> TestResult {
    let clock = Arc::new(AtomicI64::new(TEST_NOW_MS));
    let store = SelfRoleStore::with_test_clock(pool.clone(), 300, clock.clone())?;
    let older_id = "reaction:worker-a:same-ms";
    let newer_id = "reaction:worker-b:same-ms";
    let older_order = event_order_for_event_id(older_id, TEST_NOW_MS as u64);
    let newer_order = event_order_for_event_id(newer_id, TEST_NOW_MS as u64);
    assert!(older_order < newer_order);
    assert_eq!(
        event_order_for_event_id(older_id, TEST_NOW_MS as u64),
        older_order
    );
    let mut older = row(older_id, &older_order);
    older.panel_id = "same-ms".to_owned();
    let mut newer = row(newer_id, &newer_order);
    newer.panel_id = older.panel_id.clone();
    let older_event = store.claim_audit(&older).await?.expect("older event");
    let newer_event = store.claim_audit(&newer).await?.expect("newer event");
    let key = panel_key(&older);
    let older_panel = acquired(
        store
            .claim_panel(&key, Some((older_id, &older_order)))
            .await?,
    );
    assert!(matches!(
        store
            .claim_panel(&key, Some((newer_id, &newer_order)))
            .await?,
        PanelClaimResult::Busy
    ));
    assert!(store.owns_claim(&older_event).await?);
    clock.store(TEST_NOW_MS + 300, Ordering::SeqCst);
    let newer_panel = acquired(
        store
            .claim_panel(&key, Some((newer_id, &newer_order)))
            .await?,
    );
    assert_eq!(
        newer_panel.target.latest_event_id.as_deref(),
        Some(newer_id)
    );
    assert_eq!(
        newer_panel.target.latest_event_order.as_deref(),
        Some(newer_order.as_str())
    );
    assert!(!store.owns_panel_claim(&older_panel).await?);
    assert!(!store.owns_claim(&older_event).await?);
    let superseded = store
        .claim_panel(&key, Some((older_id, &older_order)))
        .await?;
    match superseded {
        PanelClaimResult::Superseded(target) => {
            assert_eq!(target.latest_event_id.as_deref(), Some(newer_id));
            assert_eq!(
                target.latest_event_order.as_deref(),
                Some(newer_order.as_str())
            );
        }
        other => panic!("older same-ms event must be superseded, got {other:?}"),
    }
    let (outcome, code, order): (String, Option<String>, String) =
        sqlx::query_as("SELECT outcome,code,event_order FROM self_role_audit WHERE event_id=$1")
            .bind(older_id)
            .fetch_one(pool)
            .await?;
    assert_eq!(outcome, "rejected");
    assert_eq!(code.as_deref(), Some("superseded_by_later_event"));
    assert_eq!(order, older_order);
    // Recovering the winner does not change its event order either.
    let recovered = store.claim_audit(&newer).await?.expect("recover winner");
    assert!(!store.owns_claim(&newer_event).await?);
    assert!(store.owns_claim(&recovered).await?);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum WaitOperation {
    EventRenewal,
    PanelRenewal,
    TargetUpdate,
}

#[derive(Debug, Clone, Copy)]
enum WaitKind {
    RowLock,
    PoolExhaustion,
}

async fn wait_past_expiry(conn: &mut PgConnection, expires: OffsetDateTime) -> TestResult {
    // Derive the wait from the database clock, including on a held pool connection.
    // The timeout bounds failures without trusting the controller's wall clock.
    tokio::time::timeout(
        Duration::from_secs(5),
        sqlx::query(
            "SELECT pg_sleep((GREATEST(0, EXTRACT(EPOCH FROM ($1::timestamptz-clock_timestamp()))) + 0.05)::double precision)",
        )
        .bind(expires)
        .execute(conn),
    )
    .await??;
    Ok(())
}

async fn exercise_db_clock_waits(pool: &PgPool) -> TestResult {
    for kind in [WaitKind::RowLock, WaitKind::PoolExhaustion] {
        for operation in [
            WaitOperation::EventRenewal,
            WaitOperation::PanelRenewal,
            WaitOperation::TargetUpdate,
        ] {
            exercise_db_clock_wait(pool, operation, kind).await?;
        }
    }
    Ok(())
}

async fn exercise_db_clock_wait(
    pool: &PgPool,
    operation: WaitOperation,
    kind: WaitKind,
) -> TestResult {
    let store = SelfRoleStore::with_lease(pool.clone(), 1_000)?;
    let suffix = format!("{kind:?}-{operation:?}");
    let mut audit = row(&format!("clock-{suffix}"), &format!("clock-order-{suffix}"));
    audit.panel_id = format!("clock-panel-{suffix}");
    let event = store.claim_audit(&audit).await?.expect("clock event");
    let key = panel_key(&audit);
    let mut panel = acquired(
        store
            .claim_panel(
                &key,
                Some((&audit.event_id, audit.event_order.as_deref().unwrap())),
            )
            .await?,
    );
    assert!(
        store
            .set_panel_claim_option(&mut panel, Some("before"))
            .await?
    );
    let (event_expiry, panel_expiry): (OffsetDateTime, OffsetDateTime) = sqlx::query_as(
        "SELECT a.processing_expires_at,p.processing_expires_at
         FROM self_role_audit a JOIN self_role_panel_claims p
         ON a.guild_id=p.guild_id AND a.member_id=p.member_id AND a.panel_id=p.panel_id
         WHERE a.event_id=$1",
    )
    .bind(&audit.event_id)
    .fetch_one(pool)
    .await?;
    let expiry = match operation {
        WaitOperation::EventRenewal => event_expiry,
        WaitOperation::PanelRenewal | WaitOperation::TargetUpdate => panel_expiry,
    };
    let mut lock = match kind {
        WaitKind::RowLock => Some(pool.begin().await?),
        WaitKind::PoolExhaustion => None,
    };
    let mut connections = Vec::new();
    if let Some(tx) = lock.as_mut() {
        match operation {
            WaitOperation::EventRenewal => {
                sqlx::query("SELECT event_id FROM self_role_audit WHERE event_id=$1 FOR UPDATE")
                    .bind(&audit.event_id)
                    .execute(&mut **tx)
                    .await?;
            }
            WaitOperation::PanelRenewal | WaitOperation::TargetUpdate => {
                sqlx::query(
                    "SELECT panel_id FROM self_role_panel_claims WHERE guild_id=$1
                     AND member_id=$2 AND panel_id=$3 FOR UPDATE",
                )
                .bind(&key.guild_id)
                .bind(&key.member_id)
                .bind(&key.panel_id)
                .execute(&mut **tx)
                .await?;
            }
        }
    } else {
        for _ in 0..TEST_POOL_SIZE {
            connections.push(tokio::time::timeout(Duration::from_secs(3), pool.acquire()).await??);
        }
        assert_eq!(connections.len(), TEST_POOL_SIZE as usize);
    }
    let conn = match lock.as_mut() {
        Some(tx) => &mut **tx,
        None => &mut *connections[0],
    };
    let (live,): (bool,) = sqlx::query_as("SELECT clock_timestamp() < $1")
        .bind(expiry)
        .fetch_one(conn)
        .await?;
    assert!(
        live,
        "lease must be live before queuing {kind:?}/{operation:?}"
    );
    let mut pending = Box::pin(async {
        match operation {
            WaitOperation::EventRenewal => store.renew_claim(&event).await,
            WaitOperation::PanelRenewal => store.renew_panel_claim(&panel).await,
            WaitOperation::TargetUpdate => {
                store
                    .set_panel_claim_option(&mut panel, Some("after"))
                    .await
            }
        }
    });
    // Poll, rather than merely constructing a future, while admission is blocked.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), pending.as_mut())
            .await
            .is_err(),
        "{kind:?}/{operation:?} unexpectedly completed while blocked"
    );
    let conn = match lock.as_mut() {
        Some(tx) => &mut **tx,
        None => &mut *connections[0],
    };
    wait_past_expiry(conn, expiry).await?;
    if let Some(tx) = lock.take() {
        tx.rollback().await?;
    }
    drop(connections); // releases all five slots in the pool-exhaustion case
    let changed = tokio::time::timeout(Duration::from_secs(3), pending.as_mut()).await??;
    assert!(!changed, "expired {kind:?}/{operation:?} must be refused");
    drop(pending);
    assert_eq!(panel.target.option_key.as_deref(), Some("before"));
    let (target, committed, current_event_expiry, current_panel_expiry):
        (Option<String>, bool, OffsetDateTime, OffsetDateTime) = sqlx::query_as(
        "SELECT p.latest_option_key,p.target_committed,a.processing_expires_at,p.processing_expires_at
         FROM self_role_audit a JOIN self_role_panel_claims p
         ON a.guild_id=p.guild_id AND a.member_id=p.member_id AND a.panel_id=p.panel_id
         WHERE a.event_id=$1",
    ).bind(&audit.event_id).fetch_one(pool).await?;
    assert_eq!(target.as_deref(), Some("before"));
    assert!(committed);
    assert_eq!(current_event_expiry, event_expiry);
    assert_eq!(current_panel_expiry, panel_expiry);
    Ok(())
}

async fn exercise_atomic_finish_wait(pool: &PgPool) -> TestResult {
    let store = SelfRoleStore::with_lease(pool.clone(), 1_000)?;
    let mut audit = row("atomic-clock-event", "atomic-clock-order");
    audit.panel_id = "atomic-clock-panel".to_owned();
    let event = store.claim_audit(&audit).await?.expect("atomic event");
    let key = panel_key(&audit);
    let mut panel = acquired(
        store
            .claim_panel(
                &key,
                Some((&audit.event_id, audit.event_order.as_deref().unwrap())),
            )
            .await?,
    );
    assert!(
        store
            .set_panel_claim_option(&mut panel, Some("before"))
            .await?
    );
    let (expiry,): (OffsetDateTime,) = sqlx::query_as(
        "SELECT processing_expires_at FROM self_role_panel_claims
         WHERE guild_id=$1 AND member_id=$2 AND panel_id=$3",
    )
    .bind(&key.guild_id)
    .bind(&key.member_id)
    .bind(&key.panel_id)
    .fetch_one(pool)
    .await?;
    let mut lock = pool.begin().await?;
    sqlx::query("SELECT event_id FROM self_role_audit WHERE event_id=$1 FOR UPDATE")
        .bind(&audit.event_id)
        .execute(&mut *lock)
        .await?;
    let (live,): (bool,) = sqlx::query_as("SELECT clock_timestamp() < $1")
        .bind(expiry)
        .fetch_one(&mut *lock)
        .await?;
    assert!(
        live,
        "panel lease must be live before waiting on the event lock"
    );
    audit.effects.added_role_ids = vec!["must-not-publish".to_owned()];
    let mut pending = Box::pin(store.finish_audit_and_set_panel_option(
        &audit,
        &event,
        &mut panel,
        Some("after"),
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), pending.as_mut())
            .await
            .is_err()
    );
    // The panel row is free initially. Its expiry must be checked AFTER the
    // event-row wait, not before updating the audit under its lock.
    wait_past_expiry(&mut lock, expiry).await?;
    lock.rollback().await?;
    assert!(!tokio::time::timeout(Duration::from_secs(3), pending.as_mut()).await??);
    drop(pending);
    assert_eq!(panel.target.option_key.as_deref(), Some("before"));
    let (target, outcome, panel_expiry): (Option<String>, String, OffsetDateTime) = sqlx::query_as(
        "SELECT p.latest_option_key,a.outcome,p.processing_expires_at
         FROM self_role_audit a JOIN self_role_panel_claims p
         ON a.guild_id=p.guild_id AND a.member_id=p.member_id AND a.panel_id=p.panel_id
         WHERE a.event_id=$1",
    )
    .bind(&audit.event_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(target.as_deref(), Some("before"));
    assert_eq!(outcome, "processing");
    assert_eq!(panel_expiry, expiry);
    assert_eq!(
        stored_effects(pool, &audit.event_id).await?,
        AuditEffects::default()
    );
    Ok(())
}
