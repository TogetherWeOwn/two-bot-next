#![cfg(test)]
//! Settings verbs through the receiver: flag gate, keyless get, claimed set,
//! idempotency replay without a second bump, and CAS without silent revert.
//! Only guarded TestDatabase pools; never staging or production.
use super::*;

const SETTINGS_KEY: &str = "TWO_RAID_JOIN_THRESHOLD";
const SETTINGS_OTHER_KEY: &str = "TWO_RAID_WINDOW_SECONDS";
const SETTINGS_ADMIN: &str = "111111111111111111";

static SETTINGS_FLAG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_settings_flag(on: bool) {
    if on {
        std::env::set_var("TWO_INTERNAL_ALLOW_SETTINGS", "1");
    } else {
        std::env::remove_var("TWO_INTERNAL_ALLOW_SETTINGS");
    }
}

fn get_payload(key: &str) -> String {
    serde_json::json!({"action": "settings.get", "key": key}).to_string()
}

fn set_payload(key: &str, value: Value, expected_version: Option<i64>) -> String {
    let mut body = serde_json::json!({"action": "settings.set", "key": key, "value": value, "updated_by": SETTINGS_ADMIN});
    if let Some(version) = expected_version {
        body["expected_version"] = json!(version);
    }
    body.to_string()
}

fn settings_app(pool: sqlx::PgPool) -> Router {
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    router(state(pool, effect))
}

#[tokio::test]
async fn settings_get_returns_current_settings_with_version() {
    let Some(db) = database().await else { return };
    let _flag = SETTINGS_FLAG_LOCK.lock().await;
    set_settings_flag(true);
    let store = two_bot_cutover::settings::SettingsStore::new(db.pool());
    store
        .set(
            staging_guild(),
            SETTINGS_KEY,
            Some(json!("8")),
            SETTINGS_ADMIN,
        )
        .await
        .unwrap();
    let expected_version: i64 = store
        .get(staging_guild(), SETTINGS_KEY)
        .await
        .unwrap()
        .expect("stored override")
        .1;
    let app = settings_app(db.pool().clone());
    let (status, headers, body) =
        answer(app.clone(), signed_read(&get_payload(SETTINGS_KEY), "old")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("idempotent-replay"));
    assert_eq!(
        body["result"],
        json!({"key": SETTINGS_KEY, "value": "8", "source": "store"})
    );
    assert_eq!(body["version"], json!(expected_version));
    let (status, _, unset) =
        answer(app, signed_read(&get_payload(SETTINGS_OTHER_KEY), "old")).await;
    set_settings_flag(false);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        unset["result"],
        json!({"key": SETTINGS_OTHER_KEY, "value": null, "source": "unset"})
    );
    assert_eq!(unset["version"], json!(0));
    db.close().await.unwrap();
}

#[tokio::test]
async fn settings_set_persists_with_flags_on_and_refused_with_flags_off() {
    let Some(db) = database().await else { return };
    let _flag = SETTINGS_FLAG_LOCK.lock().await;
    set_settings_flag(true);
    let app = settings_app(db.pool().clone());
    let raw = set_payload(SETTINGS_KEY, json!("9"), None);
    let (status, headers, body) =
        answer(app.clone(), signed(&raw, "old", "intent-settings-flag-on")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("idempotent-replay"));
    assert_eq!(
        body["result"],
        json!({"key": SETTINGS_KEY, "outcome": "saved"})
    );
    let stored = two_bot_cutover::settings::SettingsStore::new(db.pool())
        .get(staging_guild(), SETTINGS_KEY)
        .await
        .unwrap()
        .expect("persisted override");
    assert_eq!(stored.0, json!("9"));
    assert_eq!(
        body["version"],
        json!(stored.1),
        "saves return the committed CAS token"
    );
    set_settings_flag(false);
    let off = set_payload(SETTINGS_KEY, json!("10"), None);
    let (status, _, refused) = answer(app, signed(&off, "old", "intent-settings-flag-off")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(refused["error"]["code"], "action_not_allowed");
    let still = two_bot_cutover::settings::SettingsStore::new(db.pool())
        .get(staging_guild(), SETTINGS_KEY)
        .await
        .unwrap()
        .expect("kept first save");
    assert_eq!(still.0, json!("9"), "flag-off saves never reach the store");
    db.close().await.unwrap();
}

#[tokio::test]
async fn settings_set_without_idempotency_key_is_rejected() {
    let Some(db) = database().await else { return };
    let _flag = SETTINGS_FLAG_LOCK.lock().await;
    set_settings_flag(true);
    let app = settings_app(db.pool().clone());
    let raw = set_payload(SETTINGS_KEY, json!("8"), None);
    let (status, _, body) = answer(app, signed_read(&raw, "old")).await;
    set_settings_flag(false);
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "malformed");
    assert_eq!(body["error"]["retryable"], false);
    let absent = two_bot_cutover::settings::SettingsStore::new(db.pool())
        .get(staging_guild(), SETTINGS_KEY)
        .await
        .unwrap();
    assert!(absent.is_none(), "missing keys never reach the store");
    db.close().await.unwrap();
}

#[tokio::test]
async fn settings_set_replay_returns_first_result_without_second_bump() {
    let Some(db) = database().await else { return };
    let _flag = SETTINGS_FLAG_LOCK.lock().await;
    set_settings_flag(true);
    let app = settings_app(db.pool().clone());
    let raw = set_payload(SETTINGS_KEY, json!("8"), None);
    let (first_status, first_headers, first) =
        answer(app.clone(), signed(&raw, "old", "intent-settings-replay")).await;
    assert_eq!(first_status, StatusCode::OK);
    assert!(!first_headers.contains_key("idempotent-replay"));
    assert_eq!(
        first["result"],
        json!({"key": SETTINGS_KEY, "outcome": "saved"})
    );
    let store = two_bot_cutover::settings::SettingsStore::new(db.pool());
    let first_version = store
        .get(staging_guild(), SETTINGS_KEY)
        .await
        .unwrap()
        .expect("first save")
        .1;
    assert_eq!(
        first["version"],
        json!(first_version),
        "first save returns the committed CAS token"
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM guild_settings_audit WHERE guild_id = $1 AND key = $2",
    )
    .bind(staging_guild())
    .bind(SETTINGS_KEY)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(audits, 1);
    let (second_status, second_headers, second) =
        answer(app, signed(&raw, "new", "intent-settings-replay")).await;
    set_settings_flag(false);
    assert_eq!(second_status, StatusCode::OK);
    assert_eq!(second_headers["idempotent-replay"], "true");
    assert_eq!(second["result"], first["result"]);
    assert_eq!(
        second["version"], first["version"],
        "replay returns the first save's CAS token"
    );
    let second_version = store
        .get(staging_guild(), SETTINGS_KEY)
        .await
        .unwrap()
        .expect("replay keeps the row")
        .1;
    assert_eq!(
        second_version, first_version,
        "replay must not bump the version"
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM guild_settings_audit WHERE guild_id = $1 AND key = $2",
    )
    .bind(staging_guild())
    .bind(SETTINGS_KEY)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(audits, 1, "replay must not write a second audit row");
    db.close().await.unwrap();
}

#[tokio::test]
async fn settings_concurrent_save_with_stale_token_does_not_revert() {
    let Some(db) = database().await else { return };
    let _flag = SETTINGS_FLAG_LOCK.lock().await;
    set_settings_flag(true);
    let app = settings_app(db.pool().clone());
    let initial = set_payload(SETTINGS_KEY, json!("8"), Some(0));
    let (status, _, _) = answer(app.clone(), signed(&initial, "old", "intent-settings-seed")).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, current) = answer(app.clone(), signed_read(&get_payload(SETTINGS_KEY), "old")).await;
    let token = current["version"].as_i64().expect("CAS token in get");
    assert!(token < 0, "CAS tokens are negative, got {token}");
    let concurrent = set_payload(SETTINGS_KEY, json!("9"), Some(token));
    let (status, _, saved) = answer(
        app.clone(),
        signed(&concurrent, "old", "intent-settings-concurrent"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        saved["result"],
        json!({"key": SETTINGS_KEY, "outcome": "saved"})
    );
    let winner_version: i64 = two_bot_cutover::settings::SettingsStore::new(db.pool())
        .get(staging_guild(), SETTINGS_KEY)
        .await
        .unwrap()
        .expect("winner save")
        .1;
    assert_eq!(
        saved["version"],
        json!(winner_version),
        "winner save returns its committed CAS token"
    );
    let stale = set_payload(SETTINGS_KEY, json!("10"), Some(token));
    let (status, _, conflict) =
        answer(app.clone(), signed(&stale, "old", "intent-settings-stale")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["error"]["code"], "version_conflict");
    assert_eq!(conflict["error"]["retryable"], false);
    let (_, _, kept) = answer(app, signed_read(&get_payload(SETTINGS_KEY), "old")).await;
    set_settings_flag(false);
    assert_eq!(
        kept["result"],
        json!({"key": SETTINGS_KEY, "value": "9", "source": "store"}),
        "stale saves never revert the save that landed in between"
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM guild_settings_audit WHERE guild_id = $1 AND key = $2",
    )
    .bind(staging_guild())
    .bind(SETTINGS_KEY)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        audits, 2,
        "seed plus winner only; the stale loser writes no audit row"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn unwired_verbs_stay_refused_with_settings_flag_on() {
    let Some(db) = database().await else { return };
    let _flag = SETTINGS_FLAG_LOCK.lock().await;
    // `moderation.ban` and `guild.add_member` refuse on process-global flags
    // owned by sibling tests; hold their locks so a parallel flag-mutating
    // test cannot flip the verdict mid-assertion.
    let _moderation_flag = MODERATION_FLAG_LOCK.lock().await;
    let _add_member_flag = ADD_MEMBER_FLAG_LOCK.lock().await;
    set_settings_flag(true);
    let app = settings_app(db.pool().clone());
    // `role.assign` is wired by the membership slice, so it no longer belongs
    // in the unwired set: with the mock member effect it executes. The verbs
    // below stay refused because no adapter wires them (`event.upsert`) or
    // their own allowlist flag is off (`moderation.ban`, `guild.add_member`).
    for raw in [
        r#"{"action":"event.upsert","event_key":"launch","name":"Launch","starts_at":"2026-09-01T20:00:00.000Z"}"#,
        r#"{"action":"moderation.ban","discord_id":"111111111111111111","reason":"fixture reason for the ban"}"#,
        r#"{"action":"guild.add_member","discord_id":"111111111111111111","access_token":"transient-token"}"#,
    ] {
        let (status, _, body) =
            answer(app.clone(), signed(raw, "old", "intent-settings-unwired")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{raw}");
        assert_eq!(body["error"]["code"], "action_not_allowed");
    }
    set_settings_flag(false);
    db.close().await.unwrap();
}
