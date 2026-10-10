#![cfg(test)]
//! Automations verbs through the receiver: keyless redacted export, claimed
//! import with strict envelope validation, overwrite capability gating, and
//! replay without a second apply. Only guarded TestDatabase pools; never
//! staging or production.
use super::*;

const AUTOMATIONS_ACTOR: &str = "111111111111111111";

static AUTOMATIONS_FLAG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_automations_flags(on: bool, overwrite: bool) {
    if on {
        std::env::set_var("TWO_INTERNAL_ALLOW_AUTOMATIONS", "1");
    } else {
        std::env::remove_var("TWO_INTERNAL_ALLOW_AUTOMATIONS");
    }
    if on && overwrite {
        std::env::set_var("TWO_INTERNAL_ALLOW_AUTOMATIONS_OVERWRITE", "1");
    } else {
        std::env::remove_var("TWO_INTERNAL_ALLOW_AUTOMATIONS_OVERWRITE");
    }
}

fn export_payload() -> String {
    serde_json::json!({"action": "automations.export"}).to_string()
}

fn import_mee6_payload(actor: &str, overwrite: Option<bool>) -> String {
    let mut body = serde_json::json!({
        "action": "automations.import",
        "actor_id": actor,
        "commands": [
            {"command": "faq", "response": "Read the rules"},
            {"command": "welcome", "description": "says hi", "response": "hi there"},
        ],
    });
    if let Some(overwrite) = overwrite {
        body["overwrite"] = json!(overwrite);
    }
    body.to_string()
}

fn import_own_payload(actor: &str, overwrite: bool) -> String {
    serde_json::json!({
        "action": "automations.import",
        "actor_id": actor,
        "version": 1,
        "commands": [
            {"name": "faq", "description": "The faq command", "template": "Read the rules", "text_trigger": "!faq"},
        ],
        "overwrite": overwrite,
    })
    .to_string()
}

fn automations_app(pool: sqlx::PgPool) -> Router {
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    router(state(pool, effect))
}

async fn command_names(pool: &sqlx::PgPool) -> Vec<String> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM automation_commands WHERE guild_id = $1 ORDER BY name")
            .bind(staging_guild())
            .fetch_all(pool)
            .await
            .unwrap();
    rows.into_iter().map(|(name,)| name).collect()
}

async fn audit_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM automation_audit_log")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn export_returns_redacted_document_keyless() {
    let Some(db) = database().await else { return };
    let _flag = AUTOMATIONS_FLAG_LOCK.lock().await;
    set_automations_flags(true, false);
    let at = "2026-10-03T01:00:00.000Z";
    for (name, template, trigger) in [
        ("welcome", "hi there", Some("!welcome")),
        ("faq", "Read the rules", Some("!faq")),
    ] {
        two_bot_core::custom_command_store::put_command(
            db.pool(),
            &two_bot_core::custom_commands::StoredCommand {
                guild_id: staging_guild().to_owned(),
                name: name.to_owned(),
                description: format!("The {name} command"),
                template: template.to_owned(),
                text_trigger: trigger.map(str::to_owned),
                enabled: true,
            },
            AUTOMATIONS_ACTOR,
            AUTOMATIONS_ACTOR,
            at,
            at,
        )
        .await
        .unwrap();
    }
    let app = automations_app(db.pool().clone());
    // Keyless: no Idempotency-Key header is sent.
    let (status, headers, body) = answer(app, signed_read(&export_payload(), "old")).await;
    set_automations_flags(false, false);
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("idempotent-replay"));
    assert_eq!(body["result"]["version"], json!(1));
    let commands = body["result"]["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 2);
    // Ascending by name, redacted to the four wire fields only.
    assert_eq!(commands[0]["name"], json!("faq"));
    assert_eq!(commands[1]["name"], json!("welcome"));
    for command in commands {
        let keys: std::collections::HashSet<&str> = command
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["name", "description", "template", "text_trigger"]
                .into_iter()
                .collect(),
            "export carries no guild, creator, timestamp, enabled or audit material: {command}"
        );
    }
    assert!(
        !body.to_string().contains(staging_guild()),
        "guild id never reaches the wire"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn export_flag_off_is_refused_before_any_store_read() {
    let Some(db) = database().await else { return };
    let _flag = AUTOMATIONS_FLAG_LOCK.lock().await;
    set_automations_flags(false, false);
    let app = automations_app(db.pool().clone());
    let (status, _, body) = answer(app, signed_read(&export_payload(), "old")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "action_not_allowed");
    assert_eq!(body["error"]["retryable"], false);
    db.close().await.unwrap();
}

#[tokio::test]
async fn import_persists_with_flags_on_and_refused_with_flags_off() {
    let Some(db) = database().await else { return };
    let _flag = AUTOMATIONS_FLAG_LOCK.lock().await;
    set_automations_flags(true, false);
    let app = automations_app(db.pool().clone());
    let raw = import_mee6_payload(AUTOMATIONS_ACTOR, None);
    let (status, headers, body) = answer(
        app.clone(),
        signed(&raw, "old", "intent-automations-flag-on"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("idempotent-replay"));
    assert_eq!(body["result"]["imported"], json!(2));
    assert_eq!(body["result"]["skipped"], json!(0));
    assert_eq!(command_names(db.pool()).await, ["faq", "welcome"]);
    set_automations_flags(false, false);
    let off = import_mee6_payload(AUTOMATIONS_ACTOR, None);
    let (status, _, refused) =
        answer(app, signed(&off, "old", "intent-automations-flag-off")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(refused["error"]["code"], "action_not_allowed");
    assert_eq!(
        command_names(db.pool()).await,
        ["faq", "welcome"],
        "flag-off imports never reach the store"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn import_without_idempotency_key_is_rejected() {
    let Some(db) = database().await else { return };
    let _flag = AUTOMATIONS_FLAG_LOCK.lock().await;
    set_automations_flags(true, false);
    let app = automations_app(db.pool().clone());
    let raw = import_mee6_payload(AUTOMATIONS_ACTOR, None);
    let (status, _, body) = answer(app, signed_read(&raw, "old")).await;
    set_automations_flags(false, false);
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "malformed");
    assert_eq!(body["error"]["retryable"], false);
    assert!(
        command_names(db.pool()).await.is_empty(),
        "keyless imports never reach the store"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn import_malformed_bodies_are_refused_before_any_effect() {
    let Some(db) = database().await else { return };
    let _flag = AUTOMATIONS_FLAG_LOCK.lock().await;
    set_automations_flags(true, true);
    let app = automations_app(db.pool().clone());
    let bad_actor = import_mee6_payload("not-a-snowflake", None);
    let bad_overwrite = serde_json::json!({
        "action": "automations.import",
        "actor_id": AUTOMATIONS_ACTOR,
        "commands": [],
        "overwrite": "yes",
    })
    .to_string();
    let bad_version = serde_json::json!({
        "action": "automations.import",
        "actor_id": AUTOMATIONS_ACTOR,
        "version": 2,
        "commands": [],
    })
    .to_string();
    let with_schedules = serde_json::json!({
        "action": "automations.import",
        "actor_id": AUTOMATIONS_ACTOR,
        "version": 1,
        "commands": [],
        "schedules": [{"at": "soon"}],
    })
    .to_string();
    let missing_commands = serde_json::json!({
        "action": "automations.import",
        "actor_id": AUTOMATIONS_ACTOR,
    })
    .to_string();
    let missing_actor = serde_json::json!({
        "action": "automations.import",
        "commands": [],
    })
    .to_string();
    for (name, raw) in [
        ("bad_actor", bad_actor),
        ("bad_overwrite", bad_overwrite),
        ("bad_version", bad_version),
        ("with_schedules", with_schedules),
        ("missing_commands", missing_commands),
        ("missing_actor", missing_actor),
    ] {
        let (status, _, body) = answer(
            app.clone(),
            signed(&raw, "old", &format!("intent-automations-malformed-{name}")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(body["error"]["code"], "malformed", "{name}");
        assert_eq!(body["error"]["retryable"], false, "{name}");
    }
    set_automations_flags(false, false);
    assert!(
        command_names(db.pool()).await.is_empty(),
        "malformed imports never reach the store"
    );
    assert_eq!(
        audit_count(db.pool()).await,
        0,
        "malformed imports write no audit rows"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn import_success_replays_without_second_apply_but_changed_bytes_conflict() {
    let Some(db) = database().await else { return };
    let _flag = AUTOMATIONS_FLAG_LOCK.lock().await;
    set_automations_flags(true, false);
    let app = automations_app(db.pool().clone());
    let raw = import_mee6_payload(AUTOMATIONS_ACTOR, None);
    let intent = "intent-automations-replay";
    let (status, headers, first) = answer(app.clone(), signed(&raw, "old", intent)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("idempotent-replay"));
    assert_eq!(first["result"]["imported"], json!(2));
    let audits = audit_count(db.pool()).await;
    assert!(audits >= 3, "per-row plus summary audits: {audits}");
    let rows = command_names(db.pool()).await;
    assert_eq!(rows, ["faq", "welcome"]);
    // Same intent, fresh nonce and rotated key: the stored receipt replays.
    let (_, headers, replay) = answer(app.clone(), signed(&raw, "new", intent)).await;
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(
        replay["result"], first["result"],
        "replay returns the first counts verbatim"
    );
    assert_eq!(
        command_names(db.pool()).await,
        rows,
        "replay performs no second apply"
    );
    assert_eq!(
        audit_count(db.pool()).await,
        audits,
        "replay writes no second audit row"
    );
    // Same intent with changed exact signed bytes is a caller bug.
    let changed = format!("{raw} ");
    let (status, _, refusal) = answer(app, signed(&changed, "new", intent)).await;
    set_automations_flags(false, false);
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(refusal["error"]["code"], "version_conflict");
    assert_eq!(refusal["error"]["retryable"], false);
    db.close().await.unwrap();
}

#[tokio::test]
async fn import_replay_returns_first_result_after_out_of_band_row_edit() {
    let Some(db) = database().await else { return };
    let _flag = AUTOMATIONS_FLAG_LOCK.lock().await;
    set_automations_flags(true, false);
    let app = automations_app(db.pool().clone());
    let raw = import_mee6_payload(AUTOMATIONS_ACTOR, None);
    let intent = "intent-automations-replay-edit";
    let (status, _, first) = answer(app.clone(), signed(&raw, "old", intent)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["result"]["imported"], json!(2));
    let audits = audit_count(db.pool()).await;
    // An admin edits a command out of band between the first call and the
    // retry: the stored rows no longer match the first diff.
    sqlx::query("UPDATE automation_commands SET template = 'edited in discord' WHERE guild_id = $1 AND name = 'faq'")
        .bind(staging_guild())
        .execute(db.pool())
        .await
        .unwrap();
    let (_, headers, replay) = answer(app, signed(&raw, "new", intent)).await;
    set_automations_flags(false, false);
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(
        replay["result"], first["result"],
        "replay returns the first result verbatim despite the row edit"
    );
    assert_eq!(
        audit_count(db.pool()).await,
        audits,
        "replay writes no second audit row"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn import_overwrite_needs_the_separate_capability() {
    let Some(db) = database().await else { return };
    let _flag = AUTOMATIONS_FLAG_LOCK.lock().await;
    // Seed one live row with different content.
    set_automations_flags(true, false);
    let seed = import_own_payload(AUTOMATIONS_ACTOR, false);
    let (status, _, _) = answer(
        automations_app(db.pool().clone()),
        signed(&seed, "old", "intent-automations-seed"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let changed = serde_json::json!({
        "action": "automations.import",
        "actor_id": AUTOMATIONS_ACTOR,
        "version": 1,
        "commands": [
            {"name": "faq", "description": "Changed", "template": "changed answer", "text_trigger": "!faq"},
        ],
        "overwrite": true,
    })
    .to_string();
    // Overwrite requested without the capability: refused before any claim.
    let app = automations_app(db.pool().clone());
    let (status, _, refused) =
        answer(app, signed(&changed, "old", "intent-automations-deny")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(refused["error"]["code"], "action_not_allowed");
    assert_eq!(refused["error"]["retryable"], false);
    let template: String = sqlx::query_scalar(
        "SELECT template FROM automation_commands WHERE guild_id = $1 AND name = $2",
    )
    .bind(staging_guild())
    .bind("faq")
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        template, "Read the rules",
        "refused overwrite writes nothing"
    );
    // Same body with the capability applies the update.
    set_automations_flags(true, true);
    let app = automations_app(db.pool().clone());
    let (status, _, applied) =
        answer(app, signed(&changed, "old", "intent-automations-force")).await;
    set_automations_flags(false, false);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(applied["result"]["imported"], json!(1));
    let template: String = sqlx::query_scalar(
        "SELECT template FROM automation_commands WHERE guild_id = $1 AND name = $2",
    )
    .bind(staging_guild())
    .bind("faq")
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(template, "changed answer");
    db.close().await.unwrap();
}

#[tokio::test]
async fn import_export_round_trip_is_a_no_op() {
    let Some(db) = database().await else { return };
    let _flag = AUTOMATIONS_FLAG_LOCK.lock().await;
    set_automations_flags(true, false);
    let app = automations_app(db.pool().clone());
    let raw = import_own_payload(AUTOMATIONS_ACTOR, false);
    let (status, _, _) = answer(
        app.clone(),
        signed(&raw, "old", "intent-automations-roundtrip"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, exported) = answer(app, signed_read(&export_payload(), "old")).await;
    set_automations_flags(false, false);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(exported["result"]["version"], json!(1));
    assert_eq!(
        exported["result"]["commands"],
        json!([{"name": "faq", "description": "The faq command", "template": "Read the rules", "text_trigger": "!faq"}])
    );
    db.close().await.unwrap();
}
