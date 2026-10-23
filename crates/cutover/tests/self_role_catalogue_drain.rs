//! Builder regressions for retiring a live catalogue without stranding work.
use serde_json::json;
use two_bot_core::self_roles::{PanelMode, RoleOperation, SettledOutcome};
use two_bot_cutover::{
    self_role_store::{AuditEffects, SelfRoleAudit, SelfRoleStore, StoreError},
    settings::{SettingsStore, SettingsWriteError},
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "100000000000000001";
const KEY: &str = "TWO_SELF_ROLE_PANELS";

async fn database() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must run the drain regressions"
        );
        eprintln!("SKIP self-role catalogue drain: disposable test database not configured");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
            .await
            .expect("isolated migrated database"),
    )
}

fn catalogue(message: &str) -> serde_json::Value {
    json!([{
        "id":"games", "channelId":"100000000000000007", "messageId":message,
        "mode":"button", "exclusive":true,
        "options":[{"key":"new","label":"New","roleId":"100000000000000004","permissions":"0"}]
    }])
}

fn audit(event: &str) -> SelfRoleAudit {
    SelfRoleAudit {
        event_id: event.into(),
        event_order: Some(event.into()),
        guild_id: GUILD.into(),
        panel_id: "games".into(),
        member_id: "100000000000000002".into(),
        source_id: "100000000000000008".into(),
        option_key: Some("new".into()),
        role_id: Some("100000000000000004".into()),
        source: PanelMode::Button,
        operation: RoleOperation::Replace,
        outcome: SettledOutcome::Rejected,
        code: None,
        reason: None,
        effects: AuditEffects::default(),
        desired_role_ids: vec![],
        pre_mutation_role_ids: vec![],
    }
}

#[tokio::test]
async fn catalogue_replacement_removal_and_delete_refuse_until_all_work_drains() {
    let Some(db) = database().await else { return };
    let settings = SettingsStore::new(db.pool());
    let original = catalogue("100000000000000008");
    settings
        .set(GUILD, KEY, Some(original.clone()), "fixture")
        .await
        .unwrap();
    let (revision, _) = settings.poll_marks().await.unwrap();
    let roles = SelfRoleStore::new(db.pool().clone());
    let claim = roles
        .claim_pending_audit_at_revision(&audit("100000000000000100"), revision)
        .await
        .unwrap()
        .unwrap();
    assert!(
        roles
            .claim_pending_audit_at_revision(&audit("100000000000000100"), revision)
            .await
            .unwrap()
            .is_none(),
        "a real duplicate remains distinct from a stale catalogue"
    );
    for replacement in [Some(json!([])), Some(catalogue("100000000000000018")), None] {
        assert!(matches!(
            settings.set(GUILD, KEY, replacement, "fixture").await,
            Err(SettingsWriteError::SelfRoleRecoveryPending)
        ));
        assert_eq!(settings.get(GUILD, KEY).await.unwrap().unwrap().0, original);
    }
    // No-op saves do not retire anything and remain safe while work is active.
    settings
        .set(GUILD, KEY, Some(original.clone()), "fixture")
        .await
        .unwrap();
    // A terminal owner is unfinished even while its repair lease is still live.
    sqlx::query("UPDATE self_role_audit SET outcome='rejected',code='superseded_by_later_event',attempted_added_role_ids='[\"100000000000000004\"]',repair_expires_at=now()+interval '1 hour' WHERE event_id=$1")
        .bind(&claim.event_id).execute(db.pool()).await.unwrap();
    assert!(matches!(
        settings.set(GUILD, KEY, Some(json!([])), "fixture").await,
        Err(SettingsWriteError::SelfRoleRecoveryPending)
    ));
    sqlx::query(
        "UPDATE self_role_audit SET repair_complete=TRUE,exchange_pending=TRUE WHERE event_id=$1",
    )
    .bind(&claim.event_id)
    .execute(db.pool())
    .await
    .unwrap();
    assert!(matches!(
        settings.set(GUILD, KEY, None, "fixture").await,
        Err(SettingsWriteError::SelfRoleRecoveryPending)
    ));
    sqlx::query("UPDATE self_role_audit SET exchange_pending=FALSE WHERE event_id=$1")
        .bind(&claim.event_id)
        .execute(db.pool())
        .await
        .unwrap();
    settings
        .set(GUILD, KEY, Some(json!([])), "fixture")
        .await
        .unwrap();
    let stored = settings.get(GUILD, KEY).await.unwrap().unwrap();
    assert_eq!(stored.0, json!([]));
    let stale = roles
        .claim_pending_audit_at_revision(&audit("100000000000000101"), revision)
        .await;
    assert!(
        matches!(stale, Err(StoreError::CatalogueChanged)),
        "retired input is refused, not reported as an idempotent duplicate"
    );
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM self_role_audit")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 1, "stale admission wrote no processing audit");
    db.close().await.unwrap();
}

#[tokio::test]
async fn catalogue_admission_shares_the_writer_lock_and_rechecks_after_waiting() {
    let Some(db) = database().await else { return };
    let settings = SettingsStore::new(db.pool());
    settings
        .set(GUILD, KEY, Some(catalogue("100000000000000008")), "fixture")
        .await
        .unwrap();
    let (revision, _) = settings.poll_marks().await.unwrap();
    let mut writer = db.pool().begin().await.unwrap();
    sqlx::query("SELECT revision FROM guild_settings_revision WHERE singleton=TRUE FOR UPDATE")
        .fetch_one(&mut *writer)
        .await
        .unwrap();
    let roles = SelfRoleStore::new(db.pool().clone());
    let input = audit("100000000000000102");
    let admission = roles.claim_pending_audit_at_revision(&input, revision);
    tokio::pin!(admission);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut admission)
            .await
            .is_err(),
        "admission must wait behind the settings writer"
    );
    sqlx::query("UPDATE guild_settings SET value='[]'::jsonb WHERE guild_id=$1 AND key=$2")
        .bind(GUILD)
        .bind(KEY)
        .execute(&mut *writer)
        .await
        .unwrap();
    writer.commit().await.unwrap();
    assert!(
        matches!(admission.await, Err(StoreError::CatalogueChanged)),
        "revision is checked after the writer commits"
    );
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM self_role_audit")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    db.close().await.unwrap();
}
