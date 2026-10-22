#![cfg(test)]
//! Independent cross-writer and wire-validation contracts. The original
//! voice_config_acceptance manifest stays frozen.
use super::*;
use std::collections::BTreeMap;
use two_bot_core::voice_access::AccessControls;
use two_bot_core::voice_logging::{DetailLevel, LoggingSettings};
use two_bot_core::voice_rooms::CreatorChannel;
use two_bot_cutover::voice_rooms::PgRoomStore;

const ACTOR: &str = "111111111111111111";
const CHANNEL: &str = "333333333333333333";
const ROLE: &str = "222222222222222222";
const OTHER_ROLE: &str = "222222222222222223";

// The frozen suite owns a private environment lock. Run each additional HTTP
// test in its own process instead of racing that lock or editing the manifest.
// Inherit coverage and guarded database environment; set only the child flag.
fn isolated(name: &str) -> bool {
    if std::env::var("VOICE_CONFIG_FOLLOWUP_CHILD").as_deref() == Ok(name) {
        return true;
    }
    let exact = format!("internal_action_http::tests::voice_config_followup::{name}");
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
        .env("VOICE_CONFIG_FOLLOWUP_CHILD", name)
        .env("TWO_INTERNAL_ALLOW_VOICE_CONFIG", "1")
        .output()
        .expect("isolated voice contract process");
    assert!(
        output.status.success(),
        "{exact}:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "child must run exactly the requested contract"
    );
    false
}

fn document() -> Value {
    json!({
        "version": 1,
        "guild_id": staging_guild(),
        "creators": [], "templates": [], "aliases": [], "lists": [],
        "logging": null,
        "settings": {
            "creation_enabled": true, "unique_names": false,
            "no_game_label": "General", "force_single_game": false,
            "count_members_without_activity": false, "time_zone": "UTC",
            "text_channel_name": "voice-chat", "text_viewer_role_id": null,
            "command_role_id": null, "command_roles": []
        }
    })
}

fn request(config: &Value, token: Option<i64>) -> Value {
    let mut value = json!({
        "action": "voice.config.set", "config": config, "updated_by": ACTOR
    });
    if let Some(token) = token {
        value["expected_version"] = json!(token);
    }
    value
}

fn app(pool: sqlx::PgPool) -> Router {
    router(state(pool, Arc::new(MockEffect::new(MockOutcome::Success))))
}

async fn get(app: &Router) -> Value {
    let (status, _, body) = answer(
        app.clone(),
        signed_read(r#"{"action":"voice.config.get"}"#, "old"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn save(app: &Router, value: &Value, key: &str) -> Value {
    let (status, _, body) = answer(app.clone(), signed(&value.to_string(), "old", key)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"], json!({"outcome": "saved"}));
    assert!(body["version"].as_i64().is_some());
    body
}

async fn audit(pool: &sqlx::PgPool) -> Vec<(i64, String)> {
    sqlx::query_as(
        "SELECT version, actor FROM voice_settings_audit WHERE guild_id = $1 ORDER BY version",
    )
    .bind(staging_guild())
    .fetch_all(pool)
    .await
    .expect("voice audit")
}

async fn rows(pool: &sqlx::PgPool) -> Vec<Vec<String>> {
    let mut result = Vec::new();
    for table in [
        "voice_creators",
        "voice_access_controls",
        "voice_logging_settings",
        "voice_guild_settings",
        "voice_channel_templates",
        "voice_game_aliases",
        "voice_random_lists",
        "voice_random_list_choices",
        "voice_logging",
        "voice_logging_mention_members",
        "voice_logging_mention_roles",
        "voice_command_roles",
        "voice_command_role_members",
        "voice_settings_audit",
    ] {
        result.push(
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT to_jsonb(t)::text FROM {table} t ORDER BY to_jsonb(t)::text"
            )))
            .fetch_all(pool)
            .await
            .expect("complete voice row snapshot"),
        );
    }
    result
}

#[tokio::test]
async fn runtime_logging_roundtrips_each_level_and_omission_preserves_it() {
    if !isolated("runtime_logging_roundtrips_each_level_and_omission_preserves_it") {
        return;
    }
    let Some(db) = database().await else { return };
    let app = app(db.pool().clone());
    let rooms = PgRoomStore::new(db.pool().clone());
    let guild = staging_guild().parse().expect("guild ID");
    let mut config = document();
    // Import logging and runtime logging are distinct contracts, not enum aliases.
    config["logging"] = json!({
        "channel_id": CHANNEL, "detail": "verbose",
        "mention_member_ids": [ACTOR], "mention_role_ids": [ROLE]
    });
    let initial = get(&app).await;
    assert_eq!(
        initial["result"]["logging_settings"],
        json!({
            "level": "brief", "channel_id": null, "mention_role_id": null
        })
    );
    let mut token = initial["version"].as_i64().expect("initial token");
    for (index, (level, typed, channel, role)) in [
        ("off", DetailLevel::Off, Value::Null, Value::Null),
        ("brief", DetailLevel::Brief, json!(CHANNEL), json!(ROLE)),
        ("full", DetailLevel::Full, json!(CHANNEL), json!(OTHER_ROLE)),
    ]
    .into_iter()
    .enumerate()
    {
        let runtime = json!({"level": level, "channel_id": channel, "mention_role_id": role});
        let mut write = request(&config, Some(token));
        write["logging_settings"] = runtime.clone();
        let saved = save(&app, &write, &format!("voice-followup-logging-{index}")).await;
        let got = get(&app).await;
        assert_eq!(got["version"], saved["version"]);
        assert_eq!(got["result"]["logging_settings"], runtime);
        assert_eq!(got["result"]["config"]["logging"], config["logging"]);
        assert_eq!(
            rooms.logging_settings(guild).await.expect("runtime reader"),
            LoggingSettings {
                level: typed,
                channel_id: channel
                    .as_str()
                    .map(|id| id.parse().expect("decimal channel")),
                mention_role_id: role.as_str().map(|id| id.parse().expect("decimal role")),
            }
        );
        let stamped: (i64, String) = sqlx::query_as(
            "SELECT version, updated_by FROM voice_logging_settings WHERE guild_id = $1",
        )
        .bind(staging_guild())
        .fetch_one(db.pool())
        .await
        .expect("logging metadata");
        assert_eq!(
            stamped,
            (
                saved["version"].as_i64().expect("saved token"),
                ACTOR.to_owned()
            )
        );

        config["settings"]["unique_names"] = json!(index % 2 == 0);
        let omitted = save(
            &app,
            &request(&config, saved["version"].as_i64()),
            &format!("voice-followup-logging-omitted-{index}"),
        )
        .await;
        let kept = get(&app).await;
        assert_eq!(
            kept["result"]["logging_settings"], runtime,
            "omission preserves runtime logging"
        );
        assert_eq!(kept["result"]["config"]["logging"], config["logging"]);
        assert_eq!(kept["version"], omitted["version"]);
        token = omitted["version"].as_i64().expect("omission token");
    }
    assert_eq!(audit(db.pool()).await.len(), 6);
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn dashboard_access_changes_are_seen_by_the_runtime_store() {
    if !isolated("dashboard_access_changes_are_seen_by_the_runtime_store") {
        return;
    }
    let Some(db) = database().await else { return };
    let app = app(db.pool().clone());
    let rooms = PgRoomStore::new(db.pool().clone());
    let guild = staging_guild().parse().expect("guild ID");
    let mut config = document();
    config["settings"]["creation_enabled"] = json!(false);
    config["settings"]["command_role_id"] = json!(ROLE);
    config["settings"]["command_roles"] = json!([
        {"command": "kick", "role_ids": [ROLE, OTHER_ROLE]},
        {"command": "limit", "role_ids": []}
    ]);
    let first = save(&app, &request(&config, Some(0)), "voice-followup-access").await;
    assert_eq!(
        rooms.access_controls(guild).await.expect("runtime access"),
        AccessControls {
            room_creation_enabled: false,
            required_role: Some(222222222222222222),
            command_roles: BTreeMap::from([
                (
                    "kick".to_owned(),
                    vec![222222222222222222, 222222222222222223]
                ),
                ("limit".to_owned(), vec![]),
            ]),
        }
    );
    let got = get(&app).await;
    assert_eq!(got["result"]["config"]["settings"], config["settings"]);
    let cleared = save(
        &app,
        &request(&document(), first["version"].as_i64()),
        "voice-followup-access-clear",
    )
    .await;
    assert_ne!(cleared["version"], first["version"]);
    assert_eq!(
        rooms
            .access_controls(guild)
            .await
            .expect("cleared runtime access"),
        AccessControls::default()
    );
    assert_eq!(
        get(&app).await["result"]["config"]["settings"],
        document()["settings"]
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn discord_writers_invalidate_old_api_tokens_and_keep_exact_actors() {
    if !isolated("discord_writers_invalidate_old_api_tokens_and_keep_exact_actors") {
        return;
    }
    let Some(db) = database().await else { return };
    let app = app(db.pool().clone());
    let rooms = PgRoomStore::new(db.pool().clone());
    let guild = staging_guild().parse().expect("guild ID");
    save(
        &app,
        &request(&document(), Some(0)),
        "voice-followup-discord-seed",
    )
    .await;
    for (index, (table, actor)) in [
        ("voice_access_controls", "discord:access"),
        ("voice_logging_settings", "discord:logging"),
        ("voice_creators", "discord:alwaysprivate"),
    ]
    .into_iter()
    .enumerate()
    {
        let before = get(&app).await;
        match index {
            0 => rooms
                .save_access_controls(
                    guild,
                    &AccessControls {
                        room_creation_enabled: false,
                        required_role: Some(222222222222222222),
                        command_roles: BTreeMap::from([(
                            "kick".to_owned(),
                            vec![222222222222222223],
                        )]),
                    },
                )
                .await
                .expect("Discord access write"),
            1 => rooms
                .save_logging_settings(
                    guild,
                    &LoggingSettings {
                        level: DetailLevel::Full,
                        channel_id: Some(333333333333333333),
                        mention_role_id: Some(222222222222222222),
                    },
                )
                .await
                .expect("Discord logging write"),
            _ => {
                let mut creator = CreatorChannel::new(guild, 444444444444444444);
                creator.name_template = "Private @@owner@@ ##".to_owned();
                creator.private_default = true;
                rooms
                    .add_creator_with_actor(&creator, actor)
                    .await
                    .expect("attributed Discord creator write");
            }
        }
        let after = get(&app).await;
        assert_ne!(
            after["version"], before["version"],
            "{actor} invalidates API token"
        );
        assert_eq!(after["result"]["updated_by"], actor);
        let stamp: (i64, String) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT version, updated_by FROM {table} WHERE guild_id = $1"
        )))
        .bind(staging_guild())
        .fetch_one(db.pool())
        .await
        .expect("Discord row stamp");
        let token = after["version"].as_i64().expect("Discord token");
        assert_eq!(stamp, (token, actor.to_owned()));
        let audits = audit(db.pool()).await;
        assert_eq!(
            audits.len(),
            index + 2,
            "one audit per successful operation"
        );
        assert!(
            audits.contains(&(token, actor.to_owned())),
            "exact command attribution"
        );
        match index {
            0 => {
                assert_eq!(
                    after["result"]["config"]["settings"]["creation_enabled"],
                    false
                );
                assert_eq!(
                    after["result"]["config"]["settings"]["command_role_id"],
                    ROLE
                );
            }
            1 => assert_eq!(
                after["result"]["logging_settings"],
                json!({
                    "level": "full", "channel_id": CHANNEL, "mention_role_id": ROLE
                })
            ),
            _ => {
                assert_eq!(
                    after["result"]["config"]["creators"][0]["channel_id"],
                    "444444444444444444"
                );
                assert_eq!(
                    after["result"]["config"]["creators"][0]["always_private"],
                    true
                );
            }
        }
        let committed_rows = rows(db.pool()).await;
        let stale = request(&before["result"]["config"], before["version"].as_i64());
        let (status, _, body) = answer(
            app.clone(),
            signed(
                &stale.to_string(),
                "old",
                &format!("voice-followup-stale-{index}"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{actor}: {body}");
        assert_eq!(body["error"]["code"], "version_conflict");
        assert_eq!(body["error"]["retryable"], false);
        assert_eq!(
            rows(db.pool()).await,
            committed_rows,
            "stale API write leaves all rows and audit intact"
        );
        let kept = get(&app).await;
        assert_eq!(kept["version"], after["version"]);
        assert_eq!(kept["result"], after["result"]);
    }
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn replay_after_later_save_returns_first_version_without_reapplying() {
    if !isolated("replay_after_later_save_returns_first_version_without_reapplying") {
        return;
    }
    let Some(db) = database().await else { return };
    let app = app(db.pool().clone());
    let first_request = request(&document(), Some(0));
    let first = save(&app, &first_request, "voice-followup-first-intent").await;
    let mut later = document();
    later["settings"]["time_zone"] = json!("Europe/London");
    let second = save(
        &app,
        &request(&later, first["version"].as_i64()),
        "voice-followup-later-intent",
    )
    .await;
    assert_ne!(second["version"], first["version"]);
    assert_eq!(audit(db.pool()).await.len(), 2);
    let committed = rows(db.pool()).await;
    let (status, headers, replay) = answer(
        app.clone(),
        signed(
            &first_request.to_string(),
            "new",
            "voice-followup-first-intent",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(replay["result"], first["result"]);
    assert_eq!(
        replay["version"], first["version"],
        "receipt keeps the first committed version, not latest"
    );
    assert_eq!(
        rows(db.pool()).await,
        committed,
        "replay writes no configuration, row metadata or audit"
    );
    let got = get(&app).await;
    assert_eq!(got["version"], second["version"]);
    assert_eq!(
        got["result"]["config"]["settings"]["time_zone"],
        "Europe/London"
    );
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn malformed_documents_are_refused_without_voice_writes() {
    if !isolated("malformed_documents_are_refused_without_voice_writes") {
        return;
    }
    let Some(db) = database().await else { return };
    let app = app(db.pool().clone());
    let seeded = save(
        &app,
        &request(&document(), Some(0)),
        "voice-followup-validation-seed",
    )
    .await;
    let mut valid = request(&document(), seeded["version"].as_i64());
    valid["config"]["settings"]["unique_names"] = json!(true);
    // Each otherwise-real edit would land if its invalid field were ignored.
    let mut cases = Vec::new();
    for (label, pointer, value) in [
        ("unknown envelope field", "/unexpected", json!(true)),
        ("unknown document field", "/config/unexpected", json!(true)),
        (
            "unknown settings field",
            "/config/settings/unexpected",
            json!(true),
        ),
        (
            "wrong guild",
            "/config/guild_id",
            json!("1545644954272137298"),
        ),
        (
            "invalid time zone",
            "/config/settings/time_zone",
            json!("Invalid/Nowhere"),
        ),
        (
            "NUL setting",
            "/config/settings/no_game_label",
            json!("Game\0room"),
        ),
        ("NUL actor", "/updated_by", json!("111111111111111111\0")),
    ] {
        let mut invalid = valid.clone();
        let (parent, field) = pointer.rsplit_once('/').expect("test pointer");
        let object = if parent.is_empty() {
            &mut invalid
        } else {
            invalid.pointer_mut(parent).expect("fixture parent")
        };
        object[field] = value;
        cases.push((label, invalid));
    }
    let mut unknown_logging = valid.clone();
    unknown_logging["logging_settings"] = json!({"level": "full", "channel_id": CHANNEL, "mention_role_id": ROLE, "unexpected": true});
    cases.push(("unknown runtime logging field", unknown_logging));
    let mut numeric_id = valid.clone();
    numeric_id["logging_settings"] =
        json!({"level": "full", "channel_id": 333333333333333333u64, "mention_role_id": ROLE});
    cases.push(("numeric ID instead of decimal string", numeric_id));
    let baseline = get(&app).await;
    let baseline_rows = rows(db.pool()).await;
    for (index, (label, invalid)) in cases.into_iter().enumerate() {
        let (status, _, body) = answer(
            app.clone(),
            signed(
                &invalid.to_string(),
                "old",
                &format!("voice-followup-invalid-{index}"),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: {body}");
        assert_eq!(body["error"]["code"], "malformed", "{label}");
        assert_eq!(body["error"]["retryable"], false, "{label}");
        let got = get(&app).await;
        assert_eq!(got["version"], baseline["version"], "{label}");
        assert_eq!(got["result"], baseline["result"], "{label}");
        assert_eq!(
            rows(db.pool()).await,
            baseline_rows,
            "{label} changed voice state"
        );
    }
    db.close().await.expect("fixture teardown");
}
