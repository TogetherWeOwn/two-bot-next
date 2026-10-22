//! Failing-first acceptance: onboarding auto-role on join + button verify gate.
//!
//! Contract under test (builder: implement exactly this; never edit this file).
//! The builder starts from this head and makes these tests pass without
//! touching them. Every test drives the existing [`OnboardingRuntime`] seam,
//! the [`SettingsStore`] seam, or both, against a disposable migrated database
//! and a stateful loopback Discord double: no credentials, guilds, staging or
//! production resources.
//!
//! Configuration (new settings keys the builder catalogues as hot settings
//! with write-time validation):
//! - `TWO_ONBOARDING_JOIN_ROLES`: JSON array of 0-10 role ids. Each entry is
//!   a canonical nonzero snowflake string (no blanks, no leading zeros, no
//!   `@everyone`), with no duplicates. Stored arrays render comma-joined
//!   through `env_snapshot`, like every other id list in the environment.
//! - `TWO_ONBOARDING_MEMBER_ROLE`: one canonical nonzero snowflake string,
//!   never `@everyone`. Unset means the verify gate is disabled.
//! - Hostile values are refused at write time with zero stored state: no
//!   `guild_settings` row and no `guild_settings_audit` row. Refusal must be
//!   a value refusal, never `Unknown` (the keys are catalogued).
//! - Deployment (env) values work the same way; a stored row overrides the
//!   deployment value, like every other hot setting. A corrupt stored row
//!   (written around validation) fails closed: the join still succeeds and
//!   grants nothing.
//!
//! Join grants (one shared grant path on the `Welcome` job):
//! - On join the bot grants exactly the configured roles, each with a
//!   non-empty Discord audit reason: `onboarding auto-role: granted on join`.
//! - Skipped roles are never PUT and each carries an audit cause: roles at or
//!   above the bot's highest role (`skipped:above_bot`, matching the
//!   containment `>=` precedent), managed roles (`skipped:managed`), and the
//!   bot's own roles below its top (`skipped:bot_role`). Bot members get zero
//!   grants (`skipped:bot_member`). Already-held roles are not re-PUT
//!   (`skipped:already_held`), so redelivery and re-join never duplicate.
//! - `TWO_ONBOARDING_DRY_RUN=1` suppresses every join grant (the gates'
//!   documented "no role writes" promise). Unconfigured guilds stay silent.
//!
//! Verify gate (button `two:onboarding:verify`, exact id):
//! - One press grants the configured member role exactly once, with reason
//!   `onboarding verify gate: member verified`, and replies
//!   `You are now verified. Welcome!`.
//! - A second press (double-click) issues no further PUT, replies
//!   `You are already verified.`, and never errors loudly.
//! - With no member role configured the press grants nothing, replies
//!   `Verification isn't set up on this server yet.`, and never errors.
//! - The builder re-reads live member roles; the interaction snapshot's role
//!   list is never trusted for the idempotency check.
//!
//! Audit (every grant and every refusal carries a cause):
//! - Each evaluated role writes exactly one `operational_audit_log` row with
//!   `event_kind = 'onboarding_role_grant'`, the guild, the member as target,
//!   the cause as `action`, and the role id in `metadata_json.role_id`.
//! - Causes: `granted:join`, `granted:verify`, `verified:already`,
//!   `skipped:managed`, `skipped:above_bot`, `skipped:bot_role`,
//!   `skipped:bot_member`, `skipped:already_held`, `refused:unconfigured`.
//!   Precedence between overlapping skip rules is the builder's choice; the
//!   fixtures isolate one rule per role except where noted.
//!
//! Database cases run only against a guarded disposable fixture
//! (agent-testdb / CI Postgres): the `TWO_TEST_DATABASE_URL` bootstrap when
//! set, the CI-service bootstrap on CI lanes that provide the service but not
//! the var (the ignored-db lanes run `cargo test onboarding_`, which matches
//! this module), skipped locally when unset. Tests named `*_stays_silent_*`,
//! `dry_run_*` and `corrupt_*` are controls that hold before and after the
//! behavior change; every other test fails until the contract is met.
#![cfg(test)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::gateway::ensure_crypto_provider;
use crate::onboarding::{OnboardingJob, OnboardingRuntime};
use serde_json::{json, Value};
use sqlx::PgPool;
use twilight_model::application::interaction::Interaction;
use two_bot_core::onboarding::MembershipTrigger;
use two_bot_core::settings::WriteRefusal;
use two_bot_cutover::settings::{SettingsStore, SettingsWriteError};
use two_bot_discord::ActionExecutor;
use two_bot_testsupport::TestDatabase;

// --- contract vocabulary ---------------------------------------------------

const KEY_JOIN_ROLES: &str = "TWO_ONBOARDING_JOIN_ROLES";
const KEY_MEMBER_ROLE: &str = "TWO_ONBOARDING_MEMBER_ROLE";
const VERIFY_CUSTOM_ID: &str = "two:onboarding:verify";
const AUDIT_KIND: &str = "onboarding_role_grant";
const JOIN_GRANT_REASON: &str = "onboarding auto-role: granted on join";
const VERIFY_GRANT_REASON: &str = "onboarding verify gate: member verified";
const VERIFY_SUCCESS: &str = "You are now verified. Welcome!";
const VERIFY_ALREADY: &str = "You are already verified.";
const VERIFY_UNCONFIGURED: &str = "Verification isn't set up on this server yet.";

// --- fixtures ---------------------------------------------------------------

const GUILD: &str = "22";
const MEMBER: &str = "44";
const BOT_JOINER: &str = "45";
const BOT: u64 = 999;
const CHANNEL: &str = "12";
/// Role the bot holds at its top: position 5 is the grant ceiling.
const BOT_TOP: &str = "1006";
/// Role the bot holds below its top: isolates the bot-role skip rule.
const BOT_LOW: &str = "1007";
const GRANT_A: &str = "1001";
const GRANT_B: &str = "1002";
/// Position equals the bot top: `>=` blocks, like containment quarantine.
const HIGH_EQ: &str = "1003";
const HIGHER: &str = "1004";
const MANAGED: &str = "1005";
const MEMBER_ROLE: &str = "1008";
const NOW: i64 = 1_790_781_200_000;

fn role(id: &str, position: i64, managed: bool) -> Value {
    json!({
        "id": id,
        "name": format!("role-{id}"),
        "permissions": if id == GUILD {
            "1051648"
        } else if id == BOT_TOP {
            "268435456"
        } else {
            "0"
        },
        "position": position,
        "managed": managed,
    })
}

fn guild_roles() -> Vec<Value> {
    vec![
        role(GUILD, 0, false),
        role(GRANT_A, 1, false),
        role(GRANT_B, 3, false),
        role(HIGH_EQ, 5, false),
        role(HIGHER, 9, false),
        role(MANAGED, 1, true),
        role(BOT_TOP, 5, false),
        role(BOT_LOW, 2, false),
        role(MEMBER_ROLE, 1, false),
    ]
}

#[derive(Default)]
struct DiscordState {
    /// Live roles of [`MEMBER`]; PUT/DELETE mutate this, member reads serve it.
    member_roles: HashSet<String>,
}

async fn discord(state: Arc<Mutex<DiscordState>>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let mut state = state.lock().unwrap();
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("GET", "/users/@me") => {
                ScriptedResponse::json(200, json!({"id": "999", "username": "bot", "bot": true}))
            }
            ("GET", "/guilds/22") => ScriptedResponse::json(
                200,
                json!({"id": "22", "owner_id": "888", "roles": guild_roles()}),
            ),
            ("GET", "/guilds/22/roles") => {
                ScriptedResponse::json(200, Value::Array(guild_roles()))
            }
            ("GET", "/guilds/22/members/44") => ScriptedResponse::json(
                200,
                json!({"user": {"id": "44", "username": "member", "discriminator": "0"},
                       "roles": state.member_roles.iter().cloned().collect::<Vec<_>>()}),
            ),
            ("GET", "/guilds/22/members/45") => ScriptedResponse::json(
                200,
                json!({"user": {"id": "45", "username": "joiner-bot", "discriminator": "0", "bot": true},
                       "roles": []}),
            ),
            ("GET", "/guilds/22/members/46") => ScriptedResponse::json(
                200,
                json!({"user": {"id": "46", "username": "override-member", "discriminator": "0"},
                       "roles": []}),
            ),
            ("GET", "/guilds/22/members/999") => ScriptedResponse::json(
                200,
                json!({"user": {"id": "999", "username": "bot", "discriminator": "0", "bot": true},
                       "roles": [BOT_TOP, BOT_LOW]}),
            ),
            ("GET", path) if path.starts_with("/guilds/22/members/") => {
                ScriptedResponse::status(404)
            }
            ("GET", path) if path.starts_with("/channels/") => {
                let id = path.strip_prefix("/channels/").unwrap();
                ScriptedResponse::json(
                    200,
                    json!({"id": id, "guild_id": "22", "type": 0, "permission_overwrites": []}),
                )
            }
            ("PUT", path) if path.starts_with("/guilds/22/members/44/roles/") => {
                let role = path.rsplit('/').next().unwrap().to_owned();
                state.member_roles.insert(role);
                ScriptedResponse::status(204)
            }
            ("PUT", path) if path.contains("/members/") && path.contains("/roles/") => {
                ScriptedResponse::status(204)
            }
            ("DELETE", path) if path.contains("/roles/") => ScriptedResponse::status(204),
            ("POST", path) if path.starts_with("/channels/") && path.ends_with("/messages") => {
                ScriptedResponse::json(201, json!({"id": "99"}))
            }
            ("POST", path) if path.ends_with("/callback") => ScriptedResponse::status(204),
            ("PATCH", path) if path.ends_with("/messages/@original") => {
                ScriptedResponse::json(200, json!({"id": "98"}))
            }
            _ => panic!("unexpected mock Discord route: {} {}", request.method, path),
        }
    })
    .await
}

/// CI-service bootstrap for lanes that run this module without the guarded
/// var: the ignored-db lanes provide the disposable agent-testdb service (and
/// its `two_bot_test_ci` bootstrap database) but set no
/// `TWO_TEST_DATABASE_URL`. Same guard-approved shape as the `rust-tests`
/// lane's var, so the suite runs its real assertions there instead of
/// failing in setup.
const CI_SERVICE_BOOTSTRAP: &str = "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci";

/// Picks the disposable-fixture bootstrap URL: the guarded var when set, the
/// CI-service address on CI without it, `None` for a local skip. Pure so the
/// lane matrix is pinned below without a database.
fn bootstrap_url(var: Option<String>, ci: bool) -> Option<String> {
    if let Some(url) = var {
        return Some(url);
    }
    ci.then(|| CI_SERVICE_BOOTSTRAP.to_owned())
}

#[test]
fn bootstrap_prefers_the_guarded_var() {
    let url = bootstrap_url(Some("postgres://guarded".to_owned()), true).unwrap();
    assert_eq!(url, "postgres://guarded");
}

#[test]
fn bootstrap_falls_back_to_the_ci_service_without_the_var() {
    assert_eq!(
        bootstrap_url(None, true).as_deref(),
        Some(CI_SERVICE_BOOTSTRAP)
    );
}

#[test]
fn bootstrap_skips_locally_without_the_var() {
    assert_eq!(bootstrap_url(None, false), None);
}

/// Mirrors `ops_audit_acceptance_tests::database`: disposable migrated
/// fixture, skipped off-CI when the guarded URL is absent.
async fn database(test: &str) -> Option<TestDatabase> {
    let Some(url) = bootstrap_url(
        std::env::var("TWO_TEST_DATABASE_URL").ok(),
        std::env::var("GITHUB_ACTIONS").is_ok(),
    ) else {
        eprintln!("SKIP {test}: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("guarded disposable database"),
    )
}

fn vars() -> HashMap<String, String> {
    HashMap::from([
        ("DISCORD_GUILD_ID".into(), GUILD.into()),
        ("TWO_ONBOARDING_MODE".into(), "legacy".into()),
        ("TWO_ONBOARDING_DRY_RUN".into(), "0".into()),
        ("DISCORD_LANDING_CHANNEL_IDS".into(), CHANNEL.into()),
        ("DISCORD_GOODBYE_CHANNEL_IDS".into(), "13".into()),
        ("DISCORD_ANCHOR_WELCOME_CHANNEL_ID".into(), "14".into()),
        (
            "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID".into(),
            "10".into(),
        ),
        ("DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID".into(), "11".into()),
    ])
}

fn runtime(pool: &PgPool, mock: &MockRest, vars: &HashMap<String, String>) -> OnboardingRuntime {
    ensure_crypto_provider();
    let executor =
        ActionExecutor::with_proxy("onboarding-test-only-token".into(), Some(mock.origin()))
            .unwrap();
    OnboardingRuntime::new(pool.clone(), executor, vars, 22, BOT).unwrap()
}

fn welcome(member_id: u64) -> OnboardingJob {
    OnboardingJob::Welcome {
        guild_id: 22,
        member_id,
        bot: false,
        pending: false,
        trigger: MembershipTrigger::Joined { pending: false },
        roles: vec![],
    }
}

fn goodbye(member_id: u64) -> OnboardingJob {
    OnboardingJob::Goodbye {
        guild_id: 22,
        username: "member".to_owned(),
        bot: false,
        joined_at_ms: None,
        member_id: Some(member_id),
    }
}

fn verify_press(nonce: u64) -> OnboardingJob {
    let interaction: Interaction = serde_json::from_value(json!({
        "application_id": "111", "authorizing_integration_owners": {"0": "22"},
        "id": format!("500{nonce}"), "token": format!("mock-callback-{nonce}"),
        "type": 3, "version": 1, "guild_id": "22",
        "member": {"user": {"id": "44", "username": "member", "discriminator": "0"},
                   "roles": [], "deaf": false, "mute": false, "flags": 0},
        "data": {"custom_id": VERIFY_CUSTOM_ID, "component_type": 2}
    }))
    .unwrap();
    OnboardingJob::Interaction(Box::new(interaction))
}

/// Store one config row around validation, the way a legacy or corrupt row
/// would already sit in the table. Join/verify behavior tests use this so
/// their RED is behavioral (no grants), not a setup refusal; the config
/// tests pin the write path itself.
async fn store_config(pool: &PgPool, key: &str, value: Value) {
    sqlx::query(
        "INSERT INTO guild_settings (guild_id, key, value, version, updated_by)
         VALUES ('22', $1, $2, nextval('guild_settings_version_seq'), 'acceptance-fixture')",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await
    .unwrap();
}

async fn setting_rows(pool: &PgPool, key: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM guild_settings WHERE guild_id = '22' AND key = $1")
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn setting_audit_rows(pool: &PgPool, key: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM guild_settings_audit WHERE guild_id = '22' AND key = $1",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// (action, target_id, role_id) for every auto-role audit row, oldest first.
/// `role_id` is `None` exactly when the metadata carries no `role_id` key.
async fn audit_rows(pool: &PgPool) -> Vec<(String, String, Option<String>)> {
    let rows: Vec<(Option<String>, Option<String>, String)> = sqlx::query_as(
        "SELECT action, target_id, metadata_json FROM operational_audit_log
         WHERE event_kind = $1 ORDER BY created_at ASC, entry_id ASC",
    )
    .bind(AUDIT_KIND)
    .fetch_all(pool)
    .await
    .unwrap();
    rows.into_iter()
        .map(|(action, target, metadata)| {
            let parsed: Value = serde_json::from_str(&metadata).unwrap();
            assert!(parsed.is_object(), "audit metadata must be an object");
            let role = parsed
                .get("role_id")
                .map(|id| id.as_str().unwrap().to_owned());
            (action.unwrap_or_default(), target.unwrap_or_default(), role)
        })
        .collect()
}

struct Grant {
    member: String,
    role: String,
    reason: String,
}

/// Every member-role PUT with its decoded audit reason.
fn puts(mock: &MockRest) -> Vec<Grant> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "PUT" && request.path.contains("/roles/"))
        .map(|request| {
            let segments: Vec<_> = request.path.split('/').collect();
            let member = segments
                .iter()
                .position(|segment| *segment == "members")
                .map(|at| segments[at + 1].to_owned())
                .unwrap();
            let role = request.path.rsplit('/').next().unwrap().to_owned();
            let reason = request
                .header("x-audit-log-reason")
                .map(decode_reason)
                .unwrap_or_default();
            Grant {
                member,
                role,
                reason,
            }
        })
        .collect()
}

fn deletes(mock: &MockRest) -> usize {
    mock.requests()
        .iter()
        .filter(|request| request.method == "DELETE" && request.path.contains("/roles/"))
        .count()
}

fn welcome_posts(mock: &MockRest) -> usize {
    mock.requests()
        .iter()
        .filter(|request| {
            request.method == "POST"
                && request.path == format!("/api/v10/channels/{CHANNEL}/messages")
        })
        .count()
}

fn find_reply(mock: &MockRest) -> Option<Value> {
    mock.requests()
        .into_iter()
        .rfind(|request| request.method == "PATCH" && request.path.ends_with("/messages/@original"))
        .map(|patch| serde_json::from_slice(&patch.body).unwrap())
}

fn last_reply(mock: &MockRest) -> Value {
    find_reply(mock).expect("a deferred reply must be edited")
}

/// Percent-decode one `X-Audit-Log-Reason` header value.
fn decode_reason(header: &str) -> String {
    let mut bytes = Vec::new();
    let input = header.as_bytes();
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' {
            bytes.push(u8::from_str_radix(&header[index + 1..index + 3], 16).unwrap());
            index += 3;
        } else {
            bytes.push(input[index]);
            index += 1;
        }
    }
    String::from_utf8(bytes).unwrap()
}

// --- configuration -----------------------------------------------------------

#[tokio::test]
async fn join_roles_config_accepted_stored_and_cleared_while_hostile_stays_empty() {
    let Some(db) = database("join_roles_config").await else {
        return;
    };
    let pool = db.pool();
    let store = SettingsStore::new(pool);

    // Hostile values first: every refusal must leave zero stored state behind.
    // A refusal that reports `Unknown` fails: the keys are catalogued, so only
    // a value refusal proves the write-time validation exists.
    let hostile_join: Vec<(&str, Value)> = vec![
        ("bare string", json!("1001")),
        ("bare number", json!(1001)),
        ("object", json!({"roles": ["1001"]})),
        ("mixed entries", json!([GRANT_A, 1002])),
        ("null entry", json!([GRANT_A, Value::Null])),
        ("non-numeric", json!(["abc"])),
        ("zero", json!(["0"])),
        ("blank", json!([""])),
        ("unpadded mismatch", json!([" 1001 "])),
        ("leading zeros", json!(["007"])),
        ("everyone", json!([GUILD])),
        ("duplicates", json!([GRANT_A, GRANT_A])),
        (
            "over long",
            Value::Array((3001..=3011).map(|id| json!(id.to_string())).collect()),
        ),
    ];
    for (label, value) in hostile_join {
        let error = store
            .set(GUILD, KEY_JOIN_ROLES, Some(value), "acceptance-fixture")
            .await
            .expect_err(&format!(
                "hostile join-roles value must be refused: {label}"
            ));
        match error {
            SettingsWriteError::Refused(WriteRefusal::Unknown(_)) => {
                panic!("hostile join-roles value refused as Unknown ({label}): catalogue the key with value validation")
            }
            SettingsWriteError::Refused(_) => {}
            other => panic!("hostile join-roles value gave the wrong error ({label}): {other:?}"),
        }
        assert_eq!(
            setting_rows(pool, KEY_JOIN_ROLES).await,
            0,
            "refused join-roles write must store nothing ({label})"
        );
        assert_eq!(
            setting_audit_rows(pool, KEY_JOIN_ROLES).await,
            0,
            "refused join-roles write must audit nothing ({label})"
        );
    }
    let hostile_member: Vec<(&str, Value)> = vec![
        ("array", json!([MEMBER_ROLE])),
        ("number", json!(1008)),
        ("object", json!({"id": MEMBER_ROLE})),
        ("non-numeric", json!("abc")),
        ("zero", json!("0")),
        ("blank", json!("")),
        ("everyone", json!(GUILD)),
    ];
    for (label, value) in hostile_member {
        let error = store
            .set(GUILD, KEY_MEMBER_ROLE, Some(value), "acceptance-fixture")
            .await
            .expect_err(&format!(
                "hostile member-role value must be refused: {label}"
            ));
        match error {
            SettingsWriteError::Refused(WriteRefusal::Unknown(_)) => {
                panic!("hostile member-role value refused as Unknown ({label}): catalogue the key with value validation")
            }
            SettingsWriteError::Refused(_) => {}
            other => panic!("hostile member-role value gave the wrong error ({label}): {other:?}"),
        }
        assert_eq!(
            setting_rows(pool, KEY_MEMBER_ROLE).await,
            0,
            "refused member-role write must store nothing ({label})"
        );
        assert_eq!(
            setting_audit_rows(pool, KEY_MEMBER_ROLE).await,
            0,
            "refused member-role write must audit nothing ({label})"
        );
    }

    // Valid values store, read back, and clear.
    let stored = store
        .set(
            GUILD,
            KEY_JOIN_ROLES,
            Some(json!([GRANT_A, GRANT_B])),
            "acceptance-fixture",
        )
        .await
        .expect("a bounded join-roles list must be accepted once catalogued");
    assert_eq!(stored.key, KEY_JOIN_ROLES);
    assert_eq!(setting_rows(pool, KEY_JOIN_ROLES).await, 1);
    store
        .set(
            GUILD,
            KEY_MEMBER_ROLE,
            Some(json!(MEMBER_ROLE)),
            "acceptance-fixture",
        )
        .await
        .expect("a member-role id must be accepted once catalogued");
    assert_eq!(setting_rows(pool, KEY_MEMBER_ROLE).await, 1);
    // An empty list is a valid list: it stores, and grants nothing.
    store
        .set(
            GUILD,
            KEY_JOIN_ROLES,
            Some(json!(Vec::<String>::new())),
            "acceptance-fixture",
        )
        .await
        .expect("an empty join-roles list must be accepted");
    store
        .set(GUILD, KEY_JOIN_ROLES, None, "acceptance-fixture")
        .await
        .expect("clearing join-roles must be accepted");
    assert_eq!(setting_rows(pool, KEY_JOIN_ROLES).await, 0);
    store
        .set(GUILD, KEY_MEMBER_ROLE, None, "acceptance-fixture")
        .await
        .expect("clearing the member role must be accepted");
    assert_eq!(setting_rows(pool, KEY_MEMBER_ROLE).await, 0);

    db.close().await.expect("drop owned disposable database");
}

// --- join grants ---------------------------------------------------------------

#[tokio::test]
async fn join_grants_exactly_the_configured_roles() {
    let Some(db) = database("join_grants_exactly").await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
    store_config(db.pool(), KEY_JOIN_ROLES, json!([GRANT_A, GRANT_B])).await;
    runtime(db.pool(), &mock, &vars())
        .handle(welcome(44), NOW)
        .await
        .expect("a configured join must succeed");
    assert_eq!(welcome_posts(&mock), 1, "the join path must run end to end");
    let grants = puts(&mock);
    assert_eq!(
        grants
            .iter()
            .map(|grant| (grant.member.as_str(), grant.role.as_str()))
            .collect::<HashSet<_>>(),
        HashSet::from([("44", GRANT_A), ("44", GRANT_B)]),
        "join must PUT exactly the configured roles, once each"
    );
    assert!(
        grants.iter().all(|grant| grant.reason == JOIN_GRANT_REASON),
        "every join grant must carry the audit cause"
    );
    assert_eq!(deletes(&mock), 0, "join must remove no roles");
    assert_eq!(
        audit_rows(db.pool())
            .await
            .into_iter()
            .collect::<HashSet<_>>(),
        HashSet::from([
            (
                "granted:join".to_owned(),
                MEMBER.to_owned(),
                Some(GRANT_A.to_owned())
            ),
            (
                "granted:join".to_owned(),
                MEMBER.to_owned(),
                Some(GRANT_B.to_owned())
            ),
        ]),
        "every join grant must be audit-logged with its cause"
    );
    mock.shutdown().await;
    db.close().await.expect("drop owned disposable database");
}

#[tokio::test]
async fn join_skips_ungrantable_roles_with_audit_cause() {
    let Some(db) = database("join_skips_ungrantable").await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
    // One grantable role plus one role per skip rule: managed, the bot's own
    // low role, a role level with the bot top, and a role above it.
    store_config(
        db.pool(),
        KEY_JOIN_ROLES,
        json!([GRANT_B, MANAGED, BOT_LOW, HIGH_EQ, HIGHER]),
    )
    .await;
    runtime(db.pool(), &mock, &vars())
        .handle(welcome(44), NOW)
        .await
        .expect("a mixed join must succeed");
    assert_eq!(welcome_posts(&mock), 1, "the join path must run end to end");
    let grants = puts(&mock);
    assert_eq!(
        grants
            .iter()
            .map(|grant| grant.role.as_str())
            .collect::<Vec<_>>(),
        vec![GRANT_B],
        "only the grantable role may be PUT"
    );
    assert_eq!(grants[0].reason, JOIN_GRANT_REASON);
    assert_eq!(
        audit_rows(db.pool())
            .await
            .into_iter()
            .collect::<HashSet<_>>(),
        HashSet::from([
            (
                "granted:join".to_owned(),
                MEMBER.to_owned(),
                Some(GRANT_B.to_owned())
            ),
            (
                "skipped:managed".to_owned(),
                MEMBER.to_owned(),
                Some(MANAGED.to_owned())
            ),
            (
                "skipped:bot_role".to_owned(),
                MEMBER.to_owned(),
                Some(BOT_LOW.to_owned())
            ),
            (
                "skipped:above_bot".to_owned(),
                MEMBER.to_owned(),
                Some(HIGH_EQ.to_owned())
            ),
            (
                "skipped:above_bot".to_owned(),
                MEMBER.to_owned(),
                Some(HIGHER.to_owned())
            ),
        ]),
        "every evaluated role needs exactly one audit row with its cause"
    );
    mock.shutdown().await;
    db.close().await.expect("drop owned disposable database");
}

#[tokio::test]
async fn join_grants_nothing_for_bot_members() {
    let Some(db) = database("join_bot_member").await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
    store_config(db.pool(), KEY_JOIN_ROLES, json!([GRANT_A])).await;
    runtime(db.pool(), &mock, &vars())
        .handle(
            OnboardingJob::Welcome {
                guild_id: 22,
                member_id: 45,
                bot: true,
                pending: false,
                trigger: MembershipTrigger::Joined { pending: false },
                roles: vec![],
            },
            NOW,
        )
        .await
        .expect("a bot join must succeed");
    assert!(puts(&mock).is_empty(), "a bot join must grant nothing");
    assert_eq!(
        audit_rows(db.pool()).await,
        vec![("skipped:bot_member".to_owned(), BOT_JOINER.to_owned(), None)],
        "a bot join must audit-log its skip with cause"
    );
    mock.shutdown().await;
    db.close().await.expect("drop owned disposable database");
}

#[tokio::test]
async fn rejoin_never_regrants_held_roles() {
    let Some(db) = database("rejoin_no_regrant").await else {
        return;
    };
    let state = Arc::new(Mutex::new(DiscordState::default()));
    let mock = discord(Arc::clone(&state)).await;
    store_config(db.pool(), KEY_JOIN_ROLES, json!([GRANT_A])).await;
    let live = runtime(db.pool(), &mock, &vars());
    live.handle(welcome(44), NOW)
        .await
        .expect("the first join must succeed");
    assert_eq!(puts(&mock).len(), 1, "the first join grants once");
    live.handle(goodbye(44), NOW + 1)
        .await
        .expect("the goodbye must succeed");
    live.handle(welcome(44), NOW + 2)
        .await
        .expect("the re-join must succeed");
    assert_eq!(
        puts(&mock).len(),
        1,
        "the re-join must not PUT an already-held role"
    );
    assert_eq!(
        state.lock().unwrap().member_roles,
        HashSet::from([GRANT_A.to_owned()]),
        "the member holds the role exactly once"
    );
    assert_eq!(
        audit_rows(db.pool())
            .await
            .into_iter()
            .collect::<HashSet<_>>(),
        HashSet::from([
            (
                "granted:join".to_owned(),
                MEMBER.to_owned(),
                Some(GRANT_A.to_owned())
            ),
            (
                "skipped:already_held".to_owned(),
                MEMBER.to_owned(),
                Some(GRANT_A.to_owned())
            ),
        ]),
        "the re-join audits its skip instead of re-granting"
    );
    mock.shutdown().await;
    db.close().await.expect("drop owned disposable database");
}

#[tokio::test]
async fn unconfigured_guild_stays_silent_on_join() {
    let Some(db) = database("join_unconfigured").await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
    runtime(db.pool(), &mock, &vars())
        .handle(welcome(44), NOW)
        .await
        .expect("an unconfigured join must succeed");
    assert_eq!(welcome_posts(&mock), 1, "the join path must run end to end");
    assert!(puts(&mock).is_empty(), "no config means no grants");
    assert!(
        audit_rows(db.pool()).await.is_empty(),
        "no evaluated role means no audit rows"
    );
    mock.shutdown().await;
    db.close().await.expect("drop owned disposable database");
}

#[tokio::test]
async fn dry_run_suppresses_join_grants() {
    let Some(db) = database("join_dry_run").await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
    store_config(db.pool(), KEY_JOIN_ROLES, json!([GRANT_A, GRANT_B])).await;
    let mut deployment = vars();
    deployment.insert("TWO_ONBOARDING_DRY_RUN".to_owned(), "1".to_owned());
    runtime(db.pool(), &mock, &deployment)
        .handle(welcome(44), NOW)
        .await
        .expect("a dry-run join must succeed");
    assert!(puts(&mock).is_empty(), "dry run means no role writes");
    mock.shutdown().await;
    db.close().await.expect("drop owned disposable database");
}

#[tokio::test]
async fn corrupt_stored_join_config_grants_nothing() {
    let Some(db) = database("join_corrupt_config").await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
    // Written around validation, the way a legacy row predates it: the join
    // must fail closed (grant nothing, welcome normally), never fail the job.
    store_config(db.pool(), KEY_JOIN_ROLES, json!(["abc"])).await;
    runtime(db.pool(), &mock, &vars())
        .handle(welcome(44), NOW)
        .await
        .expect("a corrupt stored list must not fail the join");
    assert_eq!(welcome_posts(&mock), 1, "the welcome must still post");
    assert!(puts(&mock).is_empty(), "a corrupt list grants nothing");
    mock.shutdown().await;
    db.close().await.expect("drop owned disposable database");
}

#[tokio::test]
async fn join_roles_read_from_deployment_with_stored_override() {
    let Some(db) = database("join_roles_precedence").await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
    let mut deployment = vars();
    deployment.insert(KEY_JOIN_ROLES.to_owned(), GRANT_A.to_owned());
    runtime(db.pool(), &mock, &deployment)
        .handle(welcome(44), NOW)
        .await
        .expect("a deployment-configured join must succeed");
    assert_eq!(
        puts(&mock)
            .iter()
            .map(|grant| grant.role.as_str())
            .collect::<Vec<_>>(),
        vec![GRANT_A],
        "deployment join-roles must grant"
    );
    // A stored row overrides deployment for the same key.
    store_config(db.pool(), KEY_JOIN_ROLES, json!([GRANT_B])).await;
    runtime(db.pool(), &mock, &deployment)
        .handle(welcome(46), NOW + 1)
        .await
        .expect("a stored-override join must succeed");
    assert_eq!(
        puts(&mock)
            .iter()
            .map(|grant| (grant.member.as_str(), grant.role.as_str()))
            .collect::<HashSet<_>>(),
        HashSet::from([("44", GRANT_A), ("46", GRANT_B)]),
        "the stored row must win over deployment"
    );
    mock.shutdown().await;
    db.close().await.expect("drop owned disposable database");
}

// --- verify gate ---------------------------------------------------------------

#[tokio::test]
async fn verify_button_grants_member_role_once_and_double_press_is_idempotent() {
    let Some(db) = database("verify_idempotent").await else {
        return;
    };
    let state = Arc::new(Mutex::new(DiscordState::default()));
    let mock = discord(Arc::clone(&state)).await;
    store_config(db.pool(), KEY_MEMBER_ROLE, json!(MEMBER_ROLE)).await;
    let live = runtime(db.pool(), &mock, &vars());
    live.handle(verify_press(1), NOW)
        .await
        .expect("the first press must succeed");
    let grants = puts(&mock);
    assert_eq!(grants.len(), 1, "the first press grants exactly once");
    assert_eq!(grants[0].member, MEMBER);
    assert_eq!(grants[0].role, MEMBER_ROLE);
    assert_eq!(grants[0].reason, VERIFY_GRANT_REASON);
    assert_eq!(last_reply(&mock)["content"], json!(VERIFY_SUCCESS));
    // A double-click is a second press with a fresh interaction: no duplicate
    // grant, no loud error, and the live roles (not the interaction snapshot)
    // prove the member already holds the role.
    live.handle(verify_press(2), NOW + 1)
        .await
        .expect("the second press must succeed");
    assert_eq!(puts(&mock).len(), 1, "the double-click must not re-PUT");
    assert_eq!(
        state.lock().unwrap().member_roles,
        HashSet::from([MEMBER_ROLE.to_owned()]),
        "the member holds the role exactly once"
    );
    assert_eq!(last_reply(&mock)["content"], json!(VERIFY_ALREADY));
    assert_eq!(
        audit_rows(db.pool())
            .await
            .into_iter()
            .collect::<HashSet<_>>(),
        HashSet::from([
            (
                "granted:verify".to_owned(),
                MEMBER.to_owned(),
                Some(MEMBER_ROLE.to_owned())
            ),
            (
                "verified:already".to_owned(),
                MEMBER.to_owned(),
                Some(MEMBER_ROLE.to_owned())
            ),
        ]),
        "each press audits exactly once with its cause"
    );
    mock.shutdown().await;
    db.close().await.expect("drop owned disposable database");
}

#[tokio::test]
async fn verify_without_config_refuses_quietly() {
    let Some(db) = database("verify_unconfigured").await else {
        return;
    };
    let mock = discord(Arc::new(Mutex::new(DiscordState::default()))).await;
    runtime(db.pool(), &mock, &vars())
        .handle(verify_press(1), NOW)
        .await
        .expect("an unconfigured press must succeed");
    assert!(puts(&mock).is_empty(), "no member role means no grant");
    let reply = find_reply(&mock);
    assert!(
        reply.is_some(),
        "an unconfigured press must still answer the button"
    );
    assert_eq!(reply.unwrap()["content"], json!(VERIFY_UNCONFIGURED));
    assert_eq!(
        audit_rows(db.pool()).await,
        vec![("refused:unconfigured".to_owned(), MEMBER.to_owned(), None)],
        "an unconfigured press audits its refusal with cause"
    );
    mock.shutdown().await;
    db.close().await.expect("drop owned disposable database");
}

#[tokio::test]
async fn verify_grant_survives_rejoin_without_duplicate() {
    let Some(db) = database("verify_rejoin").await else {
        return;
    };
    let state = Arc::new(Mutex::new(DiscordState::default()));
    let mock = discord(Arc::clone(&state)).await;
    store_config(db.pool(), KEY_MEMBER_ROLE, json!(MEMBER_ROLE)).await;
    let live = runtime(db.pool(), &mock, &vars());
    live.handle(verify_press(1), NOW)
        .await
        .expect("the press must succeed");
    assert_eq!(puts(&mock).len(), 1, "the press grants once");
    // Leaving and re-joining (with no join-roles configured) never duplicates
    // the verified role and never errors.
    live.handle(goodbye(44), NOW + 1)
        .await
        .expect("the goodbye must succeed");
    live.handle(welcome(44), NOW + 2)
        .await
        .expect("the re-join must succeed");
    assert_eq!(
        puts(&mock).len(),
        1,
        "the re-join must not duplicate the verified role"
    );
    assert_eq!(
        state.lock().unwrap().member_roles,
        HashSet::from([MEMBER_ROLE.to_owned()])
    );
    assert_eq!(
        audit_rows(db.pool())
            .await
            .into_iter()
            .filter(|(action, _, _)| action.starts_with("granted:"))
            .count(),
        1,
        "no second granted row may appear"
    );
    mock.shutdown().await;
    db.close().await.expect("drop owned disposable database");
}
