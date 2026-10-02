//! Full-schema erasure acceptance, only on the guarded disposable test service.

use sqlx::PgPool;
use two_bot_cutover::member_erasure::{erase_member, plan, schema_gaps, ErasureMode};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const USER: &str = "123456789012345678";
const OTHER: &str = "123456789012345679";
const ACTOR: &str = "fixture-privacy-operator";

async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the member-erasure database suite"
            );
            eprintln!("SKIP member-erasure integration: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
            .await
            .unwrap(),
    )
}

async fn seed(pool: &PgPool) {
    sqlx::raw_sql(include_str!("fixtures/member_erasure.sql"))
        .execute(pool)
        .await
        .unwrap();
}

async fn snapshot(pool: &PgPool, table: &str) -> Vec<String> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT to_jsonb(t)::text FROM {table} t ORDER BY to_jsonb(t)::text"
    )))
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn audit_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM member_erasure_audit")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn migrated_schema_requires_every_member_column_and_detects_a_new_migration() {
    let Some(db) = database().await else { return };
    let mut conn = db.pool().acquire().await.unwrap();
    assert!(schema_gaps(&mut conn).await.unwrap().is_empty());
    sqlx::query("ALTER TABLE members ADD COLUMN recipient_user_id TEXT")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE privacy_tripwire (guild_id TEXT, observer_user_id TEXT)")
        .execute(&mut *conn)
        .await
        .unwrap();
    let gaps = schema_gaps(&mut conn).await.unwrap();
    assert!(gaps
        .iter()
        .any(|gap| gap.contains("members.recipient_user_id")));
    assert!(gaps
        .iter()
        .any(|gap| gap.contains("privacy_tripwire.observer_user_id")));
    drop(conn);
    for mode in [ErasureMode::DryRun, ErasureMode::Execute { actor: ACTOR }] {
        assert!(erase_member(db.pool(), GUILD, USER, mode).await.is_err());
    }
    assert_eq!(audit_count(db.pool()).await, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn dry_run_execute_counts_scope_survivors_and_second_run_zero() {
    let Some(db) = database().await else { return };
    seed(db.pool()).await;
    let plan = plan();
    let mut untouched = Vec::new();
    for entry in &plan.tables {
        let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT to_jsonb({})::text FROM {} WHERE NOT COALESCE(({}), false) ORDER BY to_jsonb({})::text",
            entry.table, entry.table, entry.predicate, entry.table
        )))
        .bind(GUILD).bind(USER).fetch_all(db.pool()).await.unwrap();
        // The other member and both members in the other guild survive.
        assert_eq!(
            rows.len(),
            3,
            "{} fixture must seed both guilds/members",
            entry.table
        );
        untouched.push(rows);
    }
    let mut exceptions = Vec::new();
    for table in [
        "guild_settings",
        "guild_settings_audit",
        "audit_kill_switch",
    ] {
        exceptions.push((table, snapshot(db.pool(), table).await));
    }
    let before = erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
        .await
        .unwrap();
    assert_eq!(before.len(), plan.tables.len());
    assert!(before.iter().all(|row| row.rows == 1), "{before:?}");
    assert_eq!(audit_count(db.pool()).await, 0);
    assert_eq!(
        before,
        erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
            .await
            .unwrap()
    );
    let deleted = erase_member(
        db.pool(),
        GUILD,
        USER,
        ErasureMode::Execute { actor: ACTOR },
    )
    .await
    .unwrap();
    assert_eq!(deleted, before);
    for (entry, expected) in plan.tables.iter().zip(untouched) {
        assert_eq!(
            snapshot(db.pool(), &entry.table).await,
            expected,
            "{} survivors changed",
            entry.table
        );
    }
    for (table, expected) in exceptions {
        assert_eq!(
            snapshot(db.pool(), table).await,
            expected,
            "{table} safety exception changed"
        );
    }
    assert!(erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
        .await
        .unwrap()
        .iter()
        .all(|row| row.rows == 0));
    let second = erase_member(
        db.pool(),
        GUILD,
        USER,
        ErasureMode::Execute { actor: ACTOR },
    )
    .await
    .unwrap();
    assert!(second.iter().all(|row| row.rows == 0));
    assert_eq!(audit_count(db.pool()).await, 2);
    let audits = snapshot(db.pool(), "member_erasure_audit").await;
    for audit in audits {
        let json: serde_json::Value = serde_json::from_str(&audit).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 2);
        assert_eq!(json["actor"], ACTOR);
        assert!(!audit.contains(USER) && !audit.contains(GUILD) && !audit.contains(OTHER));
    }
    db.close().await.unwrap();
}

#[tokio::test]
async fn unresolved_side_effect_guards_refuse_without_deleting_any_rows() {
    let Some(db) = database().await else { return };
    seed(db.pool()).await;
    sqlx::query("UPDATE internal_idempotency SET state = 'unknown', response_code = NULL,
        http_status = NULL, resource_id = NULL, affected = NULL WHERE guild_id = $1 AND actor_id = $2")
        .bind(GUILD).bind(USER).execute(db.pool()).await.unwrap();
    let before = erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
        .await
        .unwrap();
    assert!(erase_member(
        db.pool(),
        GUILD,
        USER,
        ErasureMode::Execute { actor: ACTOR }
    )
    .await
    .is_err());
    assert_eq!(
        before,
        erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
            .await
            .unwrap()
    );
    assert_eq!(audit_count(db.pool()).await, 0);
    sqlx::query("UPDATE internal_idempotency SET state = 'completed', response_code = 'success',
        http_status = 200, resource_id = actor_id, affected = 1 WHERE guild_id = $1 AND actor_id = $2")
        .bind(GUILD).bind(USER).execute(db.pool()).await.unwrap();
    sqlx::query("UPDATE moderation_idempotency SET state = 'in_flight' WHERE guild_id = $1")
        .bind(GUILD)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(erase_member(
        db.pool(),
        GUILD,
        USER,
        ErasureMode::Execute { actor: ACTOR }
    )
    .await
    .is_err());
    assert_eq!(
        before,
        erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
            .await
            .unwrap()
    );
    assert_eq!(audit_count(db.pool()).await, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn unsettled_automod_delivery_claims_refuse_erasure() {
    let Some(db) = database().await else { return };
    seed(db.pool()).await;
    let before = erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
        .await
        .unwrap();
    assert_eq!(
        before
            .iter()
            .find(|row| row.table == "automod_delivery_claims")
            .unwrap()
            .rows,
        1
    );
    // Started-but-uncertain: no recorded outcome, so a retry must stay blocked.
    sqlx::query("UPDATE automod_delivery_claims SET result_json = NULL, completed_at = NULL WHERE guild_id = $1 AND matched_author_id = $2")
        .bind(GUILD).bind(USER).execute(db.pool()).await.unwrap();
    assert!(erase_member(
        db.pool(),
        GUILD,
        USER,
        ErasureMode::Execute { actor: ACTOR }
    )
    .await
    .is_err());
    assert_eq!(audit_count(db.pool()).await, 0);
    assert_eq!(
        before,
        erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
            .await
            .unwrap()
    );
    // Another member's unsettled claim does not block this member's erasure.
    sqlx::query("UPDATE automod_delivery_claims SET result_json = '{}'::jsonb, completed_at = now() WHERE guild_id = $1 AND matched_author_id = $2")
        .bind(GUILD).bind(USER).execute(db.pool()).await.unwrap();
    sqlx::query("UPDATE automod_delivery_claims SET result_json = NULL, completed_at = NULL WHERE guild_id = $1 AND matched_author_id = $2")
        .bind(GUILD).bind(OTHER).execute(db.pool()).await.unwrap();
    assert_eq!(
        erase_member(
            db.pool(),
            GUILD,
            USER,
            ErasureMode::Execute { actor: ACTOR }
        )
        .await
        .unwrap(),
        before
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn unresolved_self_role_effects_and_active_claims_refuse_erasure() {
    let Some(db) = database().await else { return };
    seed(db.pool()).await;
    let before = erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
        .await
        .unwrap();
    for assignment in [
        "outcome = 'processing'",
        "unresolved_added_role_ids = '[\"999999999999999999\"]'",
        "unresolved_removed_role_ids = '[\"999999999999999999\"]'",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE self_role_audit SET {assignment} WHERE guild_id = $1 AND member_id = $2"
        )))
        .bind(GUILD)
        .bind(USER)
        .execute(db.pool())
        .await
        .unwrap();
        assert!(erase_member(
            db.pool(),
            GUILD,
            USER,
            ErasureMode::Execute { actor: ACTOR }
        )
        .await
        .is_err());
        assert_eq!(audit_count(db.pool()).await, 0);
        assert_eq!(
            before,
            erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
                .await
                .unwrap()
        );
        sqlx::query("UPDATE self_role_audit SET outcome = 'assigned', unresolved_added_role_ids = '[]', unresolved_removed_role_ids = '[]' WHERE guild_id = $1 AND member_id = $2")
            .bind(GUILD).bind(USER).execute(db.pool()).await.unwrap();
    }
    sqlx::query("UPDATE self_role_panel_claims SET processing_expires_at = now() + interval '1 hour' WHERE guild_id = $1 AND member_id = $2")
        .bind(GUILD).bind(USER).execute(db.pool()).await.unwrap();
    assert!(erase_member(
        db.pool(),
        GUILD,
        USER,
        ErasureMode::Execute { actor: ACTOR }
    )
    .await
    .is_err());
    assert_eq!(audit_count(db.pool()).await, 0);
    assert_eq!(
        before,
        erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
            .await
            .unwrap()
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn audit_failure_rolls_back_the_entire_erasure() {
    let Some(db) = database().await else { return };
    seed(db.pool()).await;
    sqlx::raw_sql(
        "CREATE FUNCTION reject_erasure_receipt() RETURNS trigger AS $$ BEGIN
        RAISE EXCEPTION 'fixture receipt unavailable'; END $$ LANGUAGE plpgsql;
        CREATE TRIGGER reject_erasure_receipt BEFORE INSERT ON member_erasure_audit
        FOR EACH ROW EXECUTE FUNCTION reject_erasure_receipt();",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let before = erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
        .await
        .unwrap();
    let mut snapshots = Vec::new();
    for entry in &plan().tables {
        snapshots.push(snapshot(db.pool(), &entry.table).await);
    }
    assert!(erase_member(
        db.pool(),
        GUILD,
        USER,
        ErasureMode::Execute { actor: ACTOR }
    )
    .await
    .is_err());
    assert_eq!(
        before,
        erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
            .await
            .unwrap()
    );
    for (entry, expected) in plan().tables.iter().zip(snapshots) {
        assert_eq!(snapshot(db.pool(), &entry.table).await, expected);
    }
    assert_eq!(audit_count(db.pool()).await, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn nested_inviter_session_keys_and_owned_children_are_included() {
    let Some(db) = database().await else { return };
    seed(db.pool()).await;
    sqlx::query("INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
        VALUES ('member_join', $3, $1, now(), 'fixture',
        '{\"nested\":{\"inviterId\":\"' || $2 || '\"}}', 'opaque-extra')")
        .bind(GUILD).bind(USER).bind(OTHER).execute(db.pool()).await.unwrap();
    sqlx::query("UPDATE community_facts SET actor_id = NULL, metadata = 'malformed session=' || $1 || ':' || $2,
        source_event_id = 'opaque-source', idempotency_key = 'opaque-key' WHERE guild_id = $1 AND actor_id = $2")
        .bind(GUILD).bind(USER).execute(db.pool()).await.unwrap();
    // Erasing a creator removes post-owned signups too, even if their subject
    // differs. The other member's separate post and other-guild rows survive.
    sqlx::query(
        "INSERT INTO lfg_signups (lfg_id, user_id, role_key, joined_at)
        VALUES ($1 || ':' || $2, $3, 'tank', now())",
    )
    .bind(GUILD)
    .bind(USER)
    .bind(OTHER)
    .execute(db.pool())
    .await
    .unwrap();
    let before = erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
        .await
        .unwrap();
    for table in ["events", "lfg_signups"] {
        assert_eq!(
            before.iter().find(|row| row.table == table).unwrap().rows,
            2
        );
    }
    assert_eq!(
        erase_member(
            db.pool(),
            GUILD,
            USER,
            ErasureMode::Execute { actor: ACTOR }
        )
        .await
        .unwrap(),
        before
    );
    assert!(erase_member(db.pool(), GUILD, USER, ErasureMode::DryRun)
        .await
        .unwrap()
        .iter()
        .all(|row| row.rows == 0));
    assert_eq!(snapshot(db.pool(), "events").await.len(), 3);
    assert_eq!(snapshot(db.pool(), "community_facts").await.len(), 3);
    assert_eq!(snapshot(db.pool(), "lfg_signups").await.len(), 3);
    db.close().await.unwrap();
}
