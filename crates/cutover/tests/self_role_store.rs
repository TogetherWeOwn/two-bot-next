//! Explicit opt-in, isolated-schema integration test. No inherited DATABASE_URL
//! or credentials: only the agent-testdb container is allowed here.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use two_bot_core::{PanelMode, RoleOperation, SettledOutcome};
use two_bot_cutover::self_role_store::{
    AuditEffects, PanelClaim, PanelClaimResult, PanelKey, SelfRoleAudit, SelfRoleStore, StoreError,
};

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

#[tokio::test]
#[ignore = "requires agent-testdb; run explicitly with --ignored"]
async fn leases_recovery_ordering_and_atomic_settlement() -> Result<(), Box<dyn std::error::Error>>
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
    let result = exercise(&pool).await;
    pool.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await?;
    admin.close().await;
    result
}

async fn exercise(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
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

    let store = SelfRoleStore::with_lease(pool.clone(), 300)?;
    assert!(matches!(
        SelfRoleStore::with_lease(pool.clone(), 0),
        Err(StoreError::InvalidLease)
    ));
    let t = OffsetDateTime::from_unix_timestamp(1_700_000_000)?;
    let audit = row("event-one", "0001");
    // Distinct connections concurrently arbitrate the same event id.
    let (a, b) = tokio::join!(store.claim_audit(&audit, t), store.claim_audit(&audit, t));
    let (a, b) = (a?, b?);
    assert_eq!(usize::from(a.is_some()) + usize::from(b.is_some()), 1);
    let original = a.or(b).expect("single winner");
    assert_eq!(original.generation, 1);
    assert_eq!(original.renew_after_ms, 100);
    assert!(store.owns_claim(&original, t).await?);
    assert!(
        store
            .renew_claim(&original, t + Duration::milliseconds(200))
            .await?
    );
    assert!(store
        .claim_audit(&audit, t + Duration::milliseconds(300))
        .await?
        .is_none());
    // Strict expiry boundary: expired claims cannot renew or authorize work.
    let expired = t + Duration::milliseconds(500);
    assert!(!store.owns_claim(&original, expired).await?);
    assert!(!store.renew_claim(&original, expired).await?);
    let mut conflicting_retry = audit.clone();
    conflicting_retry.desired_role_ids = vec!["role-z".to_owned()];
    conflicting_retry.pre_mutation_role_ids.clear();
    let recovered = store
        .claim_audit(&conflicting_retry, expired)
        .await?
        .expect("recovery");
    assert!(recovered.recovered);
    assert_eq!(recovered.generation, 2);
    assert_ne!(recovered.token, original.token);
    assert_eq!(recovered.desired_role_ids, audit.desired_role_ids);
    assert_eq!(recovered.pre_mutation_role_ids, audit.pre_mutation_role_ids);
    assert!(!store.renew_claim(&original, expired).await?);
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
    assert!(store
        .claim_audit(&audit, expired + Duration::seconds(1))
        .await?
        .is_none());
    let (outcome, desired, before, expires): (String, String, String, Option<OffsetDateTime>) = sqlx::query_as(
        "SELECT outcome,desired_role_ids,pre_mutation_role_ids,processing_expires_at FROM self_role_audit WHERE event_id='event-one'",
    ).fetch_one(pool).await?;
    assert_eq!(outcome, "switched");
    assert_eq!(desired, "[\"role-a\"]");
    assert_eq!(before, "[\"role-b\"]");
    assert!(expires.is_none());

    let key = PanelKey {
        guild_id: audit.guild_id.clone(),
        member_id: audit.member_id.clone(),
        panel_id: audit.panel_id.clone(),
    };
    let old = row("event-old", "0002");
    let old_event = store.claim_audit(&old, t).await?.expect("old event");
    let mut first = acquired(
        store
            .claim_panel(&key, Some(("event-old", "0002")), t)
            .await?,
    );
    assert!(!first.target.committed);
    assert!(
        store
            .set_panel_claim_option(&mut first, Some("go"), t)
            .await?
    );
    assert!(first.target.committed);
    let newer = row("event-new", "0003");
    let newer_event = store.claim_audit(&newer, t).await?.expect("new event");
    assert!(matches!(
        store
            .claim_panel(&key, Some(("event-new", "0003")), t)
            .await?,
        PanelClaimResult::Busy
    ));
    // A busy contender is not allowed to reject the current worker's audit.
    assert!(store.owns_claim(&old_event, t).await?);
    assert!(
        store
            .renew_panel_claim(&first, t + Duration::milliseconds(200))
            .await?
    );
    assert!(
        store
            .owns_panel_claim(&first, t + Duration::milliseconds(300))
            .await?
    );
    assert!(!store.renew_panel_claim(&first, expired).await?);
    let mut second = acquired(
        store
            .claim_panel(&key, Some(("event-new", "0003")), expired)
            .await?,
    );
    assert_eq!(second.generation, 2);
    assert_eq!(second.target.option_key.as_deref(), Some("go"));
    assert!(second.target.committed); // uncommitted incoming intent never overwrites go
    assert!(!store.release_panel_claim(&first, expired).await?);
    assert!(
        !store
            .set_panel_claim_option(&mut first, Some("chess"), expired)
            .await?
    );
    assert!(!store.renew_claim(&old_event, expired).await?);
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
            .claim_panel(&key, Some(("event-stale", "0001")), expired)
            .await?,
        PanelClaimResult::Superseded(_)
    ));

    // A stale event must roll back the panel target publication as well.
    assert!(matches!(
        store
            .finish_audit_and_set_panel_option(
                &old,
                &old_event,
                &mut second,
                Some("chess"),
                expired
            )
            .await,
        Err(StoreError::StaleClaim)
    ));
    let (target,): (Option<String>,) = sqlx::query_as(
        "SELECT latest_option_key FROM self_role_panel_claims WHERE guild_id='test-guild' AND member_id='test-member' AND panel_id='games'",
    ).fetch_one(pool).await?;
    assert_eq!(target.as_deref(), Some("go"));
    // Recover the current event first; publishing requires its exact fence.
    let newer_recovered = store
        .claim_audit(&newer, expired)
        .await?
        .expect("recover current event");
    assert!(!store.owns_claim(&newer_event, expired).await?);
    let mut settled = newer.clone();
    settled.effects.added_role_ids = vec!["role-a".to_owned()];
    settled.effects.removed_role_ids = vec!["role-b".to_owned()];
    assert!(
        store
            .finish_audit_and_set_panel_option(
                &settled,
                &newer_recovered,
                &mut second,
                Some("chess"),
                expired
            )
            .await?
    );
    assert_eq!(second.target.option_key.as_deref(), Some("chess"));
    assert!(store.release_panel_claim(&second, expired).await?);
    // A maintenance claim retains chronology and the last committed target.
    let mut repair = acquired(store.claim_panel(&key, None, expired).await?);
    assert_eq!(repair.target.latest_event_order.as_deref(), Some("0003"));
    assert_eq!(repair.target.option_key.as_deref(), Some("chess"));
    assert!(
        store
            .set_panel_claim_option(&mut repair, None, expired)
            .await?
    );
    assert!(repair.target.committed && repair.target.option_key.is_none());
    assert!(store.release_panel_claim(&repair, expired).await?);
    let empty = acquired(store.claim_panel(&key, None, expired).await?);
    assert!(empty.target.committed && empty.target.option_key.is_none());

    // Independent panels do not contend; same lane has one winner even when
    // the incoming event ids differ.
    let other = PanelKey {
        panel_id: "other".to_owned(),
        ..key.clone()
    };
    let (a, b) = tokio::join!(
        store.claim_panel(&other, Some(("a", "0010")), t),
        store.claim_panel(&other, Some(("b", "0010")), t)
    );
    assert_eq!(
        usize::from(matches!(a?, PanelClaimResult::Acquired(_)))
            + usize::from(matches!(b?, PanelClaimResult::Acquired(_))),
        1
    );
    assert!(store.owns_panel_claim(&empty, expired).await?);
    Ok(())
}
