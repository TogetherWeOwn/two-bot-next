#![cfg(test)]
//! Acceptance: `voice.config.get` / `voice.config.set` internal-action verbs
//! (dashboard parity slice 1a).
//!
//! CONTRACT the build card implements. Frozen: the Builder never edits this
//! file and implements the slice until every test here passes.
//!
//! * Verbs `voice.config.get` (read, keyless) and `voice.config.set` (write,
//!   durable idempotency key required) over `PgRoomStore`/`PgVoiceConfigStore`,
//!   allowlisted like `settings.*` behind `TWO_INTERNAL_ALLOW_VOICE_CONFIG=1`.
//! * Whole-config shape: get returns
//!   `{"result":{"config":{...VoiceConfiguration}},"version":N}`; set takes
//!   `{"action":"voice.config.set","config":{...},"updated_by":"<actor>",
//!   "expected_version":N?}` and returns
//!   `{"result":{"outcome":"saved"},"version":M}`. A fresh guild reads the
//!   import defaults with `version: 0`, mirroring unset `settings.get`.
//! * Versions are guild-level `BIGINT`s. A set without `expected_version` is a
//!   blind write; a stale one is a non-retryable 409 `version_conflict` that
//!   never reverts the winner and writes no audit row. Token shape (negative
//!   CAS tokens or row versions) is the builder's choice; only the behavior
//!   is pinned.
//! * Attribution is required: a missing or empty `updated_by` is a 400
//!   `malformed` that writes nothing. The actor *format* (`dashboard:<id>`
//!   vs snowflake) is the builder's choice; these tests send snowflakes.
//! * Migration: `version`/`updated_by` columns on the voice tables and an
//!   append-only `voice_settings_audit` table (spec section 3.5 name) with
//!   `guild_id`/`actor`/`at` columns. One audit row per successful set;
//!   replays and stale losers write none. Backfill attribution for
//!   pre-migration rows is the accepted gap from the parent card.
//! * `voice.config.set` without an `Idempotency-Key` header is a 400
//!   `malformed`, mirroring `settings.set`.
//!
//! RED today: the verbs are not implemented, so every HTTP test below fails on
//! its first status assertion with 403 `action_not_allowed`, the allowlist
//! pins fail on `is_implemented` / `needs_idempotency_key`, and the migration
//! probes fail naming the missing columns/table. Only
//! `voice_flag_does_not_leak_into_settings_verbs` passes (control).
//! Only guarded TestDatabase pools; never staging or production.
use super::*;
use two_bot_core::internal_actions::{is_implemented, needs_idempotency_key, InternalFlags};

const VOICE_FLAG: &str = "TWO_INTERNAL_ALLOW_VOICE_CONFIG";
const VOICE_ADMIN: &str = "111111111111111111";
const VOICE_CREATOR: &str = "222222222222222222";
const VOICE_TEMPLATE_CHANNEL: &str = "333333333333333333";

static VOICE_FLAG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_voice_flag(on: bool) {
    if on {
        std::env::set_var(VOICE_FLAG, "1");
    } else {
        std::env::remove_var(VOICE_FLAG);
    }
}

fn voice_app(pool: sqlx::PgPool) -> Router {
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    router(state(pool, effect))
}

/// Minimal structurally-valid configuration: no creators, templates, aliases,
/// lists or logging, and default guild settings. Mirrors the
/// `PgVoiceConfigStore` fresh-guild defaults.
fn base_config() -> Value {
    json!({
        "version": two_bot_core::voice_config::VOICE_CONFIG_VERSION,
        "guild_id": staging_guild(),
        "creators": [],
        "templates": [],
        "aliases": [],
        "lists": [],
        "logging": null,
        "settings": {
            "creation_enabled": true,
            "unique_names": false,
            "no_game_label": "General",
            "force_single_game": false,
            "count_members_without_activity": false,
            "time_zone": "UTC",
            "text_channel_name": "voice-chat",
            "text_viewer_role_id": null,
            "command_role_id": null,
            "command_roles": []
        }
    })
}

fn creator_entry(channel_id: &str, name_template: &str) -> Value {
    json!({
        "channel_id": channel_id,
        "name_template": name_template,
        "status_template": null,
        "default_limit": 4,
        "always_private": false,
        "text_channels": true,
        "position": "above",
        "first_number": 1,
        "group_by_category": false,
        "permission_source": {"kind": "creator"}
    })
}

fn get_payload() -> String {
    serde_json::json!({"action": "voice.config.get"}).to_string()
}

fn set_payload(config: &Value, expected_version: Option<i64>) -> String {
    let mut body = serde_json::json!({
        "action": "voice.config.set",
        "config": config,
        "updated_by": VOICE_ADMIN,
    });
    if let Some(version) = expected_version {
        body["expected_version"] = json!(version);
    }
    body.to_string()
}

async fn audit_rows(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM voice_settings_audit WHERE guild_id = $1 AND actor = $2",
    )
    .bind(staging_guild())
    .bind(VOICE_ADMIN)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[test]
fn voice_verbs_are_implemented_and_join_on_their_own_flag() {
    assert!(is_implemented("voice.config.get"));
    assert!(is_implemented("voice.config.set"));
    let off = InternalFlags::from_map(&HashMap::new());
    assert!(!off.is_enabled("voice.config.get"));
    assert!(!off.is_enabled("voice.config.set"));
    let on = InternalFlags::from_map(&HashMap::from([(VOICE_FLAG.to_owned(), "1".to_owned())]));
    assert!(on.is_enabled("voice.config.get"));
    assert!(on.is_enabled("voice.config.set"));
}

#[test]
fn voice_config_set_needs_an_idempotency_key_and_get_does_not() {
    assert!(needs_idempotency_key("voice.config.set"));
    assert!(!needs_idempotency_key("voice.config.get"));
}

#[test]
fn voice_flag_does_not_leak_into_settings_verbs() {
    // Control: the new flag gates only the voice verbs, and the settings flag
    // gates only the settings verbs. Green before and after the slice.
    let voice_only =
        InternalFlags::from_map(&HashMap::from([(VOICE_FLAG.to_owned(), "1".to_owned())]));
    assert!(!voice_only.is_enabled("settings.get"));
    assert!(!voice_only.is_enabled("settings.set"));
    let settings_only = InternalFlags::from_map(&HashMap::from([(
        "TWO_INTERNAL_ALLOW_SETTINGS".to_owned(),
        "1".to_owned(),
    )]));
    assert!(!settings_only.is_enabled("voice.config.get"));
    assert!(!settings_only.is_enabled("voice.config.set"));
}

#[tokio::test]
async fn voice_config_get_returns_default_config_with_zero_version_on_fresh_guild() {
    let Some(db) = database().await else { return };
    let _flag = VOICE_FLAG_LOCK.lock().await;
    set_voice_flag(true);
    let app = voice_app(db.pool().clone());
    let (status, _, body) = answer(app, signed_read(&get_payload(), "old")).await;
    set_voice_flag(false);
    assert_eq!(status, StatusCode::OK);
    let config = &body["result"]["config"];
    assert_eq!(
        config["version"],
        json!(two_bot_core::voice_config::VOICE_CONFIG_VERSION)
    );
    assert_eq!(config["guild_id"], json!(staging_guild()));
    assert_eq!(config["creators"], json!([]));
    assert_eq!(config["templates"], json!([]));
    assert_eq!(config["aliases"], json!([]));
    assert_eq!(config["lists"], json!([]));
    assert!(config["logging"].is_null());
    assert_eq!(config["settings"]["time_zone"], json!("UTC"));
    assert_eq!(config["settings"]["unique_names"], json!(false));
    assert_eq!(body["version"], json!(0));
    db.close().await.unwrap();
}

#[tokio::test]
async fn voice_config_set_persists_with_flag_on_and_refused_with_flag_off() {
    let Some(db) = database().await else { return };
    let _flag = VOICE_FLAG_LOCK.lock().await;
    set_voice_flag(true);
    let app = voice_app(db.pool().clone());
    let mut config = base_config();
    config["settings"]["unique_names"] = json!(true);
    let raw = set_payload(&config, None);
    let (status, headers, body) =
        answer(app.clone(), signed(&raw, "old", "intent-voice-flag-on")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("idempotent-replay"));
    assert_eq!(body["result"], json!({"outcome": "saved"}));
    let saved_version = body["version"].as_i64().expect("set returns a version");
    let (_, _, got) = answer(app.clone(), signed_read(&get_payload(), "old")).await;
    assert_eq!(
        got["result"]["config"]["settings"]["unique_names"],
        json!(true)
    );
    assert_eq!(got["version"], json!(saved_version));
    set_voice_flag(false);
    let mut off_config = base_config();
    off_config["settings"]["unique_names"] = json!(false);
    let off = set_payload(&off_config, None);
    let (status, _, refused) =
        answer(app.clone(), signed(&off, "old", "intent-voice-flag-off")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(refused["error"]["code"], "action_not_allowed");
    // Flag-off refusals never reach the store: re-enable and re-read.
    set_voice_flag(true);
    let (_, _, kept) = answer(app, signed_read(&get_payload(), "old")).await;
    set_voice_flag(false);
    assert_eq!(
        kept["result"]["config"]["settings"]["unique_names"],
        json!(true),
        "flag-off saves never reach the store"
    );
    assert_eq!(kept["version"], json!(saved_version));
    db.close().await.unwrap();
}

#[tokio::test]
async fn voice_config_set_without_idempotency_key_is_rejected() {
    let Some(db) = database().await else { return };
    let _flag = VOICE_FLAG_LOCK.lock().await;
    set_voice_flag(true);
    let app = voice_app(db.pool().clone());
    let raw = set_payload(&base_config(), None);
    let (status, _, body) = answer(app.clone(), signed_read(&raw, "old")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "malformed");
    assert_eq!(body["error"]["retryable"], false);
    let (status, _, got) = answer(app, signed_read(&get_payload(), "old")).await;
    set_voice_flag(false);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        got["version"],
        json!(0),
        "keyless saves never reach the store"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn voice_config_set_without_actor_is_refused_without_write() {
    let Some(db) = database().await else { return };
    let _flag = VOICE_FLAG_LOCK.lock().await;
    set_voice_flag(true);
    let app = voice_app(db.pool().clone());
    let config = base_config();
    for (n, raw) in [
        serde_json::json!({"action": "voice.config.set", "config": config.clone()}).to_string(),
        serde_json::json!({
            "action": "voice.config.set",
            "config": config,
            "updated_by": "",
        })
        .to_string(),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, _, body) = answer(
            app.clone(),
            signed(&raw, "old", &format!("intent-voice-no-actor-{n}")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}");
        assert_eq!(body["error"]["code"], "malformed");
    }
    let (status, _, got) = answer(app, signed_read(&get_payload(), "old")).await;
    set_voice_flag(false);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        got["version"],
        json!(0),
        "actorless saves never reach the store"
    );
    assert_eq!(
        audit_rows(db.pool()).await,
        0,
        "actorless saves write no audit row"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn voice_config_set_replay_returns_first_result_without_second_bump_or_audit() {
    let Some(db) = database().await else { return };
    let _flag = VOICE_FLAG_LOCK.lock().await;
    set_voice_flag(true);
    let app = voice_app(db.pool().clone());
    let raw = set_payload(&base_config(), None);
    let (first_status, first_headers, first) =
        answer(app.clone(), signed(&raw, "old", "intent-voice-replay")).await;
    assert_eq!(first_status, StatusCode::OK);
    assert!(!first_headers.contains_key("idempotent-replay"));
    assert_eq!(first["result"], json!({"outcome": "saved"}));
    assert!(
        first["version"].as_i64().is_some(),
        "first save returns a version"
    );
    assert_eq!(audit_rows(db.pool()).await, 1);
    let (second_status, second_headers, second) =
        answer(app.clone(), signed(&raw, "new", "intent-voice-replay")).await;
    assert_eq!(second_status, StatusCode::OK);
    assert_eq!(second_headers["idempotent-replay"], "true");
    assert_eq!(second["result"], first["result"]);
    assert_eq!(
        second["version"], first["version"],
        "replay returns the first save's version"
    );
    let (_, _, got) = answer(app, signed_read(&get_payload(), "old")).await;
    set_voice_flag(false);
    assert_eq!(
        got["version"], first["version"],
        "replay must not bump the version"
    );
    assert_eq!(
        audit_rows(db.pool()).await,
        1,
        "replay must not write a second audit row"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn voice_config_set_with_stale_expected_version_conflicts_without_revert_or_audit() {
    let Some(db) = database().await else { return };
    let _flag = VOICE_FLAG_LOCK.lock().await;
    set_voice_flag(true);
    let app = voice_app(db.pool().clone());
    let seed = set_payload(&base_config(), Some(0));
    let (status, _, _) = answer(app.clone(), signed(&seed, "old", "intent-voice-cas-seed")).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, current) = answer(app.clone(), signed_read(&get_payload(), "old")).await;
    let token = current["version"].as_i64().expect("version in get");
    let mut winner_config = base_config();
    winner_config["settings"]["time_zone"] = json!("Europe/London");
    let winner = set_payload(&winner_config, Some(token));
    let (status, _, saved) = answer(
        app.clone(),
        signed(&winner, "old", "intent-voice-cas-winner"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["result"], json!({"outcome": "saved"}));
    let winner_version = saved["version"].as_i64().expect("winner version");
    assert_ne!(winner_version, token, "a save that lands bumps the version");
    let mut stale_config = base_config();
    stale_config["settings"]["time_zone"] = json!("UTC");
    let stale = set_payload(&stale_config, Some(token));
    let (status, _, conflict) =
        answer(app.clone(), signed(&stale, "old", "intent-voice-cas-stale")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["error"]["code"], "version_conflict");
    assert_eq!(conflict["error"]["retryable"], false);
    let (_, _, kept) = answer(app, signed_read(&get_payload(), "old")).await;
    set_voice_flag(false);
    assert_eq!(
        kept["result"]["config"]["settings"]["time_zone"],
        json!("Europe/London"),
        "stale saves never revert the save that landed in between"
    );
    assert_eq!(kept["version"], json!(winner_version));
    assert_eq!(
        audit_rows(db.pool()).await,
        2,
        "seed plus winner only; the stale loser writes no audit row"
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn voice_config_set_round_trips_import_file_only_keys() {
    let Some(db) = database().await else { return };
    let _flag = VOICE_FLAG_LOCK.lock().await;
    set_voice_flag(true);
    let app = voice_app(db.pool().clone());
    let mut config = base_config();
    config["creators"] = json!([creator_entry(VOICE_CREATOR, "Hall of {owner}")]);
    config["templates"] = json!([{
        "channel_id": VOICE_TEMPLATE_CHANNEL,
        "name_template": "Lounge {n}",
        "status_template": null,
    }]);
    config["aliases"] = json!([{"game": "Chess", "alias": "Big Brain Time"}]);
    config["settings"]["time_zone"] = json!("Europe/London");
    config["settings"]["unique_names"] = json!(true);
    let raw = set_payload(&config, None);
    let (status, _, _) = answer(app.clone(), signed(&raw, "old", "intent-voice-import-keys")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, got) = answer(app, signed_read(&get_payload(), "old")).await;
    set_voice_flag(false);
    assert_eq!(status, StatusCode::OK);
    let round_tripped = &got["result"]["config"];
    assert_eq!(
        round_tripped["creators"][0]["name_template"],
        json!("Hall of {owner}")
    );
    assert_eq!(
        round_tripped["templates"][0]["name_template"],
        json!("Lounge {n}")
    );
    assert_eq!(
        round_tripped["aliases"],
        json!([{"game": "Chess", "alias": "Big Brain Time"}])
    );
    assert_eq!(
        round_tripped["settings"]["time_zone"],
        json!("Europe/London")
    );
    assert_eq!(round_tripped["settings"]["unique_names"], json!(true));
    // The dashboard write lands in the import-file-only rows, not just the
    // read-back projection.
    let stored_template: String =
        sqlx::query_scalar("SELECT name_template FROM voice_channel_templates WHERE guild_id = $1")
            .bind(staging_guild())
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(stored_template, "Lounge {n}");
    let stored_alias: String =
        sqlx::query_scalar("SELECT alias FROM voice_game_aliases WHERE guild_id = $1")
            .bind(staging_guild())
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(stored_alias, "Big Brain Time");
    let stored_creator: String =
        sqlx::query_scalar("SELECT name_template FROM voice_creators WHERE guild_id = $1")
            .bind(staging_guild())
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(stored_creator, "Hall of {owner}");
    let stored_settings: (String, bool) = sqlx::query_as(
        "SELECT time_zone, unique_names FROM voice_guild_settings WHERE guild_id = $1",
    )
    .bind(staging_guild())
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(stored_settings, ("Europe/London".to_owned(), true));
    db.close().await.unwrap();
}

#[tokio::test]
async fn voice_tables_carry_row_versions_and_an_append_only_audit() {
    let Some(db) = database().await else { return };
    for table in [
        "voice_creators",
        "voice_access_controls",
        "voice_logging_settings",
        "voice_guild_settings",
    ] {
        for column in ["version", "updated_by"] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                 WHERE table_name = $1 AND column_name = $2)",
            )
            .bind(table)
            .bind(column)
            .fetch_one(db.pool())
            .await
            .unwrap();
            assert!(exists, "{table} must carry a {column} column");
        }
    }
    for column in ["guild_id", "actor", "at"] {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns
             WHERE table_name = 'voice_settings_audit' AND column_name = $1)",
        )
        .bind(column)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert!(exists, "voice_settings_audit must carry a {column} column");
    }
    let _flag = VOICE_FLAG_LOCK.lock().await;
    set_voice_flag(true);
    let app = voice_app(db.pool().clone());
    let raw = set_payload(&base_config(), None);
    let (status, _, body) =
        answer(app.clone(), signed(&raw, "old", "intent-voice-row-version")).await;
    assert_eq!(status, StatusCode::OK);
    let committed = body["version"].as_i64().expect("set returns a version");
    let row: (i64, String) =
        sqlx::query_as("SELECT version, updated_by FROM voice_guild_settings WHERE guild_id = $1")
            .bind(staging_guild())
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(row.0, committed, "row version matches the wire version");
    assert_eq!(row.1, VOICE_ADMIN, "row records the actor");
    assert_eq!(audit_rows(db.pool()).await, 1);
    let deleted = sqlx::query("DELETE FROM voice_settings_audit WHERE guild_id = $1")
        .bind(staging_guild())
        .execute(db.pool())
        .await;
    set_voice_flag(false);
    assert!(
        deleted.is_err(),
        "voice_settings_audit must be append-only like guild_settings_audit"
    );
    db.close().await.unwrap();
}
