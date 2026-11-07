//! Historical publication identity comes from frozen audit scalars, never maps.
#![cfg(feature = "db")]

use std::collections::HashMap;
use two_bot_core::botlog::{assemble_botlog_page, validate_botlog_read, BotlogPage};
use two_bot_core::internal_action_store::{
    AuditSubject, DiscordId, EventOutcome, ExecutionClaim, InternalActionStore, InternalClaim,
    InternalStoreError, ReconciliationEvidence, RequestIdentity, TerminalResponse,
};
use two_bot_core::internal_actions::{MemberOutcome, RoleAssignmentReport};
use two_bot_core::RankKey;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "100000000000000001";
const USER: &str = "100000000000000002";
const ROLE: &str = "100000000000000003";
const EVENT: &str = "100000000000000004";
const REPLACEMENT: &str = "100000000000000005";

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap()
}

fn identity(key: &str, action: &str) -> RequestIdentity {
    RequestIdentity::new("website", &format!("botlog:{key}"), action, key.as_bytes()).unwrap()
}

fn subject(event_key: Option<&str>, role: bool) -> AuditSubject {
    AuditSubject {
        guild_id: Some(DiscordId::new(GUILD).unwrap()),
        target_id: Some(DiscordId::new(USER).unwrap()),
        resolved_role_id: role.then(|| DiscordId::new(ROLE).unwrap()),
        audit_event_key: event_key.map(str::to_owned),
        ..AuditSubject::default()
    }
}

async fn claim(
    store: &InternalActionStore,
    id: &RequestIdentity,
    subject: &AuditSubject,
) -> ExecutionClaim {
    match store.claim(id, subject).await.unwrap() {
        InternalClaim::Claimed(claim) => claim,
        other => panic!("expected claim, got {other:?}"),
    }
}

fn receipt(outcome: Option<EventOutcome>) -> TerminalResponse {
    TerminalResponse::Success {
        resource_id: outcome.map(|_| DiscordId::new(EVENT).unwrap()),
        affected: 1,
        outcome,
        stop_reason: None,
    }
}

async fn page(store: &InternalActionStore) -> BotlogPage {
    assemble_botlog_page(
        store.list_botlog_page(GUILD, 0, 0, 200).await.unwrap(),
        &HashMap::new(),
        &validate_botlog_read(&serde_json::Map::new()).unwrap(),
    )
}

#[tokio::test]
async fn event_history_survives_replacement_repointing_and_duplicate_reverse_maps() {
    let db = database().await;
    let store = InternalActionStore::new(db.pool().clone());
    for (key, action, outcome) in [
        ("opened", "event.upsert", EventOutcome::Created),
        ("updated", "event.upsert", EventOutcome::Updated),
        ("cancelled", "event.cancel", EventOutcome::Cancelled),
    ] {
        let id = identity(key, action);
        let claim = claim(&store, &id, &subject(Some("original:key"), false)).await;
        store.finish(&claim, &receipt(Some(outcome))).await.unwrap();
    }
    let before = page(&store).await;
    assert_eq!(before.entries.len(), 3);
    assert!(before
        .entries
        .iter()
        .all(|e| e.event_key.as_deref() == Some("original:key")));
    store
        .put_event_key(GUILD, "original:key", REPLACEMENT)
        .await
        .unwrap();
    store
        .put_event_key(GUILD, "different-key", EVENT)
        .await
        .unwrap();
    store
        .put_event_key(GUILD, "also-different", EVENT)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(page(&store).await).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    let replay = store
        .claim(
            &identity("opened", "event.upsert"),
            &subject(Some("different-key"), false),
        )
        .await
        .unwrap();
    assert!(
        matches!(replay, InternalClaim::Replay(result) if result == receipt(Some(EventOutcome::Created)))
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn drained_legacy_events_retry_without_enriching_audit_identity() {
    let db = database().await;
    let store = InternalActionStore::new(db.pool().clone());
    for (action, outcome) in [
        ("event.upsert", EventOutcome::Created),
        ("event.cancel", EventOutcome::Cancelled),
    ] {
        // An old image omits the new key scalar, then proves no REST dispatch
        // during drain. The payload hash still binds the exact event request.
        let other_action = if action == "event.upsert" {
            "event.cancel"
        } else {
            "event.upsert"
        };
        let key = format!("botlog:drained:{action}");
        let id = RequestIdentity::new("website", &key, action, b"original-request").unwrap();
        let first = claim(&store, &id, &subject(None, false)).await;
        let intent_id = first.intent_id();
        store.release_proven_not_sent(first).await.unwrap();
        let retry_subject = subject(Some("launch"), false);
        for changed in [
            RequestIdentity::new("website", &key, action, b"changed-request").unwrap(),
            RequestIdentity::new("website", &key, other_action, b"original-request").unwrap(),
        ] {
            assert!(matches!(
                store.claim(&changed, &retry_subject).await.unwrap(),
                InternalClaim::Mismatch
            ));
        }
        let mut wrong_subject = retry_subject.clone();
        wrong_subject.target_id = Some(DiscordId::new(REPLACEMENT).unwrap());
        assert!(matches!(
            store.claim(&id, &wrong_subject).await.unwrap(),
            InternalClaim::Mismatch
        ));
        let retry = claim(&store, &id, &retry_subject).await;
        assert_eq!(retry.intent_id(), intent_id);
        // A second no-dispatch release must stay retryable too.
        store.release_proven_not_sent(retry).await.unwrap();
        let retry = claim(&store, &id, &retry_subject).await;
        store.finish(&retry, &receipt(Some(outcome))).await.unwrap();
        assert!(matches!(
            store.claim(&id, &retry_subject).await.unwrap(),
            InternalClaim::Replay(result) if result == receipt(Some(outcome))
        ));
        let unchanged: bool = sqlx::query_scalar(
            "SELECT audit_event_key IS NULL AND NOT EXISTS (
                SELECT 1 FROM internal_action_log WHERE intent_id = $1 AND audit_event_key IS NOT NULL
             ) FROM internal_idempotency WHERE intent_id = $1",
        )
        .bind(intent_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert!(
            unchanged,
            "legacy intent and every audit keep their NULL identity"
        );
    }
    assert!(
        page(&store).await.entries.is_empty(),
        "unnamed history stays private"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn event_reclaim_and_reconciliation_never_relabel_pinned_or_legacy_identity() {
    let db = database().await;
    let store = InternalActionStore::new(db.pool().clone());
    let id = identity("reclaim", "event.upsert");
    let pinned = subject(Some("original:key"), false);
    let first = claim(&store, &id, &pinned).await;
    store.release_proven_not_sent(first).await.unwrap();
    assert!(matches!(
        store
            .claim(&id, &subject(Some("wrong:key"), false))
            .await
            .unwrap(),
        InternalClaim::Mismatch
    ));
    let first = claim(&store, &id, &pinned).await;
    store.mark_unknown(&first).await.unwrap();
    assert!(matches!(
        store
            .resolve_event_intent(
                first.intent_id(),
                "event.upsert",
                GUILD,
                Some(("wrong:key", EVENT)),
                &receipt(Some(EventOutcome::Created)),
                ReconciliationEvidence::DiscordConfirmedEffect
            )
            .await,
        Err(InternalStoreError::TransitionRefused)
    ));
    assert_eq!(
        store.event_id_for_key(GUILD, "wrong:key").await.unwrap(),
        None
    );
    store
        .resolve_event_intent(
            first.intent_id(),
            "event.upsert",
            GUILD,
            Some(("original:key", EVENT)),
            &receipt(Some(EventOutcome::Created)),
            ReconciliationEvidence::DiscordConfirmedEffect,
        )
        .await
        .unwrap();
    let legacy = claim(
        &store,
        &identity("legacy", "event.upsert"),
        &subject(None, false),
    )
    .await;
    store.mark_unknown(&legacy).await.unwrap();
    store
        .resolve_event_intent(
            legacy.intent_id(),
            "event.upsert",
            GUILD,
            Some(("legacy:key", REPLACEMENT)),
            &TerminalResponse::Success {
                resource_id: Some(DiscordId::new(REPLACEMENT).unwrap()),
                affected: 1,
                outcome: Some(EventOutcome::Updated),
                stop_reason: None,
            },
            ReconciliationEvidence::DiscordConfirmedEffect,
        )
        .await
        .unwrap();
    let page = page(&store).await;
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].event_key.as_deref(), Some("original:key"));
    assert_ne!(
        page.next, page.entries[0].seq,
        "unnamed legacy candidates still advance consumption"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn rank_proof_is_bound_to_the_claim_and_atomic_with_terminal_audit() {
    let db = database().await;
    let store = InternalActionStore::new(db.pool().clone());
    let id = identity("rank-proof", "role.assign");
    let claim = claim(&store, &id, &subject(None, true)).await;
    let mut report = RoleAssignmentReport {
        outcome: MemberOutcome::Assigned,
        resolved_role_id: REPLACEMENT.to_owned(),
        rank_key: Some(RankKey::Prospect),
    };
    assert!(matches!(
        store
            .finish_role_assignment(&claim, &receipt(None), &report)
            .await,
        Err(InternalStoreError::TransitionRefused)
    ));
    report.resolved_role_id = ROLE.to_owned();
    for outcome in [
        MemberOutcome::Added,
        MemberOutcome::AlreadyMember,
        MemberOutcome::AlreadyHeld,
    ] {
        report.outcome = outcome;
        assert!(matches!(
            store
                .finish_role_assignment(&claim, &receipt(None), &report)
                .await,
            Err(InternalStoreError::InvalidInput)
        ));
    }
    report.outcome = MemberOutcome::Assigned;
    sqlx::query("ALTER TABLE internal_action_log RENAME TO fixture_hidden_audit")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(matches!(
        store
            .finish_role_assignment(&claim, &receipt(None), &report)
            .await,
        Err(InternalStoreError::Unavailable)
    ));
    let state: (String, Option<String>) = sqlx::query_as(
        "SELECT state, audit_rank_key FROM internal_idempotency WHERE intent_id = $1",
    )
    .bind(claim.intent_id())
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        state,
        ("in_flight".into(), None),
        "no partial metadata commit"
    );
    sqlx::query("ALTER TABLE fixture_hidden_audit RENAME TO internal_action_log")
        .execute(db.pool())
        .await
        .unwrap();
    store.mark_unknown(&claim).await.unwrap();
    store
        .reconcile(
            &id,
            &receipt(None),
            ReconciliationEvidence::DiscordConfirmedEffect,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE rank_ladder SET role_id = $1 WHERE rank_key = 'prospect'")
        .bind(ROLE)
        .execute(db.pool())
        .await
        .unwrap();
    let page = page(&store).await;
    assert!(
        page.entries.is_empty(),
        "a later ladder cannot reconstruct lost executor evidence"
    );
    assert_ne!(
        page.next,
        two_bot_core::botlog::BotlogCursor::zero().encode()
    );
    assert!(matches!(
        store
            .finish_role_assignment(&claim, &receipt(None), &report)
            .await,
        Err(InternalStoreError::TransitionRefused)
    ));
    db.close().await.unwrap();
}

#[tokio::test]
async fn audit_identity_bounds_immutability_and_erasure_are_enforced() {
    let db = database().await;
    let store = InternalActionStore::new(db.pool().clone());
    let id = identity("event", "event.upsert");
    for key in [
        "".to_owned(),
        "x".repeat(201),
        "spaces are not keys".into(),
        "é".into(),
    ] {
        assert!(matches!(
            store.claim(&id, &subject(Some(&key), false)).await,
            Err(InternalStoreError::InvalidInput)
        ));
    }
    assert!(matches!(
        store
            .claim(
                &identity("wrong-action", "role.assign"),
                &subject(Some("event"), true)
            )
            .await,
        Err(InternalStoreError::InvalidInput)
    ));
    let claim = claim(&store, &id, &subject(Some("original:key"), false)).await;
    store
        .finish(&claim, &receipt(Some(EventOutcome::Created)))
        .await
        .unwrap();
    for sql in [
        "UPDATE internal_idempotency SET audit_event_key = 'changed' WHERE intent_id = $1",
        "UPDATE internal_idempotency SET audit_event_key = NULL WHERE intent_id = $1",
        "UPDATE internal_action_log SET audit_event_key = 'changed' WHERE intent_id = $1",
        "UPDATE internal_action_log SET created_at = clock_timestamp() WHERE intent_id = $1",
        "UPDATE internal_action_log SET affected = 0 WHERE intent_id = $1",
        "UPDATE internal_action_log SET action = 'role.assign' WHERE intent_id = $1",
    ] {
        assert!(sqlx::query(sql)
            .bind(claim.intent_id())
            .execute(db.pool())
            .await
            .is_err());
    }
    for (column, value) in [
        ("audit_event_key", "x".repeat(201)),
        ("audit_event_key", "é".into()),
        ("audit_rank_key", "commander".into()),
        ("audit_rank_key", "prospect".into()),
    ] {
        let sql = format!("INSERT INTO internal_action_log (intent_id, phase, caller_hash, action, {column}) SELECT intent_id, 'unknown', caller_hash, action, $2 FROM internal_idempotency WHERE intent_id = $1");
        let error = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(claim.intent_id())
            .bind(value)
            .execute(db.pool())
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
    }
    let deleted = sqlx::query("DELETE FROM internal_action_log WHERE intent_id = $1")
        .bind(claim.intent_id())
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(
        deleted.rows_affected(),
        2,
        "member erasure DELETE remains permitted"
    );
    db.close().await.unwrap();
}
