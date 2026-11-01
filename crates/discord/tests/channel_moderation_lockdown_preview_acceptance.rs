//! Slice 4 acceptance: `/lockdown-preview` slash with pending store, audit
//! row and rate limiter, still unpublished (dark).
//!
//! Contract under test (spec sections 3-6, build slice 4):
//!
//! * `lockdown-preview` takes exactly one option (`reason`), gated on
//!   `ManageChannels`. Router/validation refusals happen before any Discord
//!   read and write no audit row.
//! * Success path: at most one channel GET plus one guild-roles GET, then one
//!   saved-lockdown store read, then exactly one `moderation.lockdown` /
//!   `lockdown_preview` audit row with numbers-only metadata and an empty
//!   reason, then (armed only) one pending entry. A redelivered interaction id
//!   writes no second row and renders nothing.
//! * Unarmed previews (live state already matches the lockdown plan) audit but
//!   store no pending entry and carry no digest.
//! * Rate budget: 10 previews per actor and 30 per guild per 10 minutes,
//!   charged after validation and before any read. Refused attempts cost no
//!   GETs and no audit rows.
//! * Every path is GET-only over HTTP; the reason is never echoed.

#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::Row;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{ChannelModerationStore, InteractionRouter, RouterGates};
use two_bot_discord::{
    register_channel_handlers, ActionExecutor, ChannelModerationRuntime, ChannelReply,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "111111111111111111";
const CHANNEL: &str = "222222222222222222";
const ACTOR: &str = "333333333333333333";
const KEEPER_ROLE: &str = "444444444444444444";
const MANAGE_CHANNELS: u64 = 1 << 4;
const MANAGE_MESSAGES_ONLY: u64 = 1 << 13;

struct Database {
    store: ChannelModerationStore,
    fixture: TestDatabase,
}

impl Database {
    async fn open() -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("explicit isolated test service required");
        let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated disposable fixture");
        let store = ChannelModerationStore::from_pool(fixture.pool().clone());
        Self { store, fixture }
    }

    async fn audit_count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM moderation_audit")
            .fetch_one(self.store.pool())
            .await
            .unwrap()
    }

    async fn close(self) {
        self.fixture.close().await.expect("verify fixture teardown");
    }
}

fn router(enabled: bool) -> InteractionRouter {
    let mut router = InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD.parse().unwrap()),
        moderation: enabled,
        voice: false,
        voice_assistant: false,
        scorecard: false,
        automations: false,
        announcements: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    });
    register_channel_handlers(&mut router);
    router
}

fn interaction(id: u64, options: Value, permissions: u64) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "444444444444444444", "type": 2,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0, "permissions": permissions.to_string()},
        "data": {"id": "555555555555555555", "type": 1, "name": "lockdown-preview", "options": options},
        "member": {"user": {"id": ACTOR, "username": "test-member", "discriminator": "0", "avatar": null},
                   "roles": [], "flags": 0, "deaf": false, "mute": false, "permissions": permissions.to_string()},
        "token": "synthetic-interaction-token", "version": 1, "locale": "en-US", "guild_locale": "en-US",
        "authorizing_integration_owners": {"0": GUILD}, "entitlements": []
    })).expect("synthetic Twilight interaction")
}

fn reason_options() -> Value {
    json!([{"name": "reason", "type": 3, "value": "raid drill"}])
}

fn runtime(db: &Database, mock: &MockRest) -> ChannelModerationRuntime {
    ChannelModerationRuntime::new(
        db.store.clone(),
        ActionExecutor::with_proxy("synthetic-test-token".to_owned(), Some(mock.origin())).unwrap(),
    )
}

fn handled(reply: Option<ChannelReply>, command: &str) -> ChannelReply {
    reply.unwrap_or_else(|| {
        panic!("{command} is not handled by ChannelModerationRuntime yet (RED: unimplemented read)")
    })
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// Everyone overwrite as read live: allow VIEW, deny nothing, so the plan is
/// armed. One keeper role holds SendMessages with no channel overwrite.
fn armed_channel_doc() -> Value {
    json!({
        "id": CHANNEL,
        "type": 0,
        "guild_id": GUILD,
        "permission_overwrites": [{"id": GUILD, "type": 0, "allow": "1024", "deny": "0"}],
    })
}

/// Everyone overwrite already denies every lockdown bit: the plan is unarmed.
fn unarmed_channel_doc() -> Value {
    json!({
        "id": CHANNEL,
        "type": 0,
        "guild_id": GUILD,
        "permission_overwrites": [{"id": GUILD, "type": 0, "allow": "0", "deny": "377957124160"}],
    })
}

fn roles_doc() -> Value {
    json!([
        {"id": GUILD, "position": 0, "permissions": "104324673"},
        {"id": KEEPER_ROLE, "position": 1, "permissions": "3072"},
    ])
}

fn everyone_only_roles_doc() -> Value {
    json!([{"id": GUILD, "position": 0, "permissions": "104324673"}])
}

/// Exactly the success-path Discord traffic: one channel GET, one roles GET.
fn assert_preview_gets(mock: &MockRest) {
    let gets: Vec<_> = mock
        .requests()
        .iter()
        .filter(|r| r.method == "GET")
        .map(|r| r.path.clone())
        .collect();
    assert_eq!(
        gets,
        vec![
            format!("/api/v10/channels/{CHANNEL}"),
            format!("/api/v10/guilds/{GUILD}/roles"),
        ],
        "one channel GET plus one roles GET, in order"
    );
    assert!(
        mock.requests().iter().all(|r| r.method == "GET"),
        "previews never mutate over HTTP"
    );
}

fn assert_no_requests(mock: &MockRest) {
    assert!(
        mock.requests().is_empty(),
        "a refused preview must not touch Discord: {:?}",
        mock.requests()
            .iter()
            .map(|r| (r.method.clone(), r.path.clone()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn accepts_lockdown_preview_command() {
    let request = interaction(1, reason_options(), MANAGE_CHANNELS);
    assert!(
        ChannelModerationRuntime::accepts(&request),
        "RED: lockdown-preview is not routed to the channel slice yet"
    );
}

#[tokio::test]
async fn preview_armed_renders_diff_audits_once_and_arms_pending() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let request = interaction(100, reason_options(), MANAGE_CHANNELS);
    let reply = handled(
        runtime.execute(&router(true), &request).await.unwrap(),
        "lockdown-preview",
    );
    assert_eq!(reply.outcome, "lockdown_preview");
    assert!(
        reply.text.contains("Lockdown preview"),
        "render header: {}",
        reply.text
    );
    assert!(
        reply.text.contains("nothing was changed"),
        "render admits no mutation: {}",
        reply.text
    );
    assert!(
        reply
            .text
            .contains("Confirm re-reads live permissions and refuses if they changed"),
        "render discloses the confirm divergence rule: {}",
        reply.text
    );
    assert!(
        reply.text.contains("@everyone overwrite:"),
        "render shows the exact @everyone diff: {}",
        reply.text
    );
    assert!(
        !reply.text.contains("raid drill"),
        "the audit reason is never echoed: {}",
        reply.text
    );
    let digest = reply
        .digest32
        .clone()
        .expect("armed previews carry a digest");
    assert_eq!(digest.len(), 32, "digest32 is the first 32 hex chars");
    assert!(
        digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "digest32 is hex: {digest}"
    );
    assert_preview_gets(&mock);
    assert_eq!(db.audit_count().await, 1, "exactly one audit row");
    let row = sqlx::query(
        "SELECT action, outcome, reason, request_id, idempotency_key, metadata_json, \
                guild_id, actor_id, channel_id FROM moderation_audit",
    )
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("action"), "moderation.lockdown");
    assert_eq!(row.get::<String, _>("outcome"), "lockdown_preview");
    assert_eq!(row.get::<String, _>("reason"), "");
    assert_eq!(row.get::<String, _>("request_id"), "100");
    assert_eq!(row.get::<String, _>("idempotency_key"), "100");
    assert_eq!(row.get::<String, _>("guild_id"), GUILD);
    assert_eq!(row.get::<String, _>("actor_id"), ACTOR);
    assert_eq!(row.get::<String, _>("channel_id"), CHANNEL);
    let metadata: Value =
        serde_json::from_str(row.get::<String, _>("metadata_json").as_str()).unwrap();
    let object = metadata.as_object().expect("metadata is a JSON object");
    assert_eq!(
        object
            .keys()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>(),
        [
            "prior_exists",
            "roles_evaluated",
            "roles_keeping_send",
            "member_overwrite_allows"
        ]
        .into_iter()
        .collect::<std::collections::HashSet<_>>(),
        "metadata is numbers-only per the ledger discipline"
    );
    assert!(
        object.values().all(Value::is_number),
        "metadata values are numbers: {metadata}"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn preview_unarmed_audits_without_pending_or_buttons() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, unarmed_channel_doc()),
            ScriptedResponse::json(200, everyone_only_roles_doc()),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let request = interaction(101, reason_options(), MANAGE_CHANNELS);
    let reply = handled(
        runtime.execute(&router(true), &request).await.unwrap(),
        "lockdown-preview",
    );
    assert_eq!(reply.outcome, "lockdown_preview");
    assert!(
        reply.digest32.is_none(),
        "unarmed previews carry no digest, so respond attaches no buttons"
    );
    assert_preview_gets(&mock);
    assert_eq!(db.audit_count().await, 1, "unarmed previews still audit");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn preview_validation_refusals_cost_no_reads_and_no_rows() {
    let db = Database::open().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    // Missing reason, wrong-only option, and an extra option beside reason.
    for (id, options) in [
        (102, json!([])),
        (103, json!([{"name": "count", "type": 4, "value": 5}])),
        (
            104,
            json!([
                {"name": "reason", "type": 3, "value": "drill"},
                {"name": "count", "type": 4, "value": 5},
            ]),
        ),
    ] {
        let request = interaction(id, options, MANAGE_CHANNELS);
        let reply = handled(
            runtime.execute(&router(true), &request).await.unwrap(),
            "lockdown-preview",
        );
        assert_eq!(reply.outcome, "refused");
        assert!(reply.digest32.is_none());
    }
    assert_no_requests(&mock);
    assert_eq!(
        db.audit_count().await,
        0,
        "validation refusals write no audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn preview_refuses_non_staff_wrong_guild_and_disabled_without_effects() {
    let db = Database::open().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    // Nobody (0) and ManageMessages-only staff: preview needs ManageChannels.
    for (id, permissions) in [(105, 0), (106, MANAGE_MESSAGES_ONLY)] {
        let request = interaction(id, reason_options(), permissions);
        let reply = handled(
            runtime.execute(&router(true), &request).await.unwrap(),
            "lockdown-preview",
        );
        assert_eq!(reply.outcome, "refused");
    }
    let mut wrong_guild = interaction(107, reason_options(), MANAGE_CHANNELS);
    wrong_guild.guild_id = Some(twilight_model::id::Id::new(999));
    let reply = handled(
        runtime.execute(&router(true), &wrong_guild).await.unwrap(),
        "lockdown-preview",
    );
    assert_eq!(reply.outcome, "refused");
    let request = interaction(108, reason_options(), MANAGE_CHANNELS);
    let reply = handled(
        runtime.execute(&router(false), &request).await.unwrap(),
        "lockdown-preview",
    );
    assert_eq!(reply.outcome, "refused");
    assert_no_requests(&mock);
    assert_eq!(db.audit_count().await, 0);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn preview_read_failure_writes_no_audit_row() {
    let db = Database::open().await;
    // Channel GET fails: no roles read, no audit row, retry guidance.
    let mock = MockRest::start(
        vec![ScriptedResponse::status(503)],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let request = interaction(109, reason_options(), MANAGE_CHANNELS);
    let reply = handled(
        runtime.execute(&router(true), &request).await.unwrap(),
        "lockdown-preview",
    );
    assert_eq!(reply.outcome, "refused");
    assert!(
        contains_ci(&reply.text, "try again"),
        "read failure carries retry guidance: {}",
        reply.text
    );
    assert_eq!(db.audit_count().await, 0, "failed reads audit nothing");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn preview_store_outage_is_an_error_not_a_row() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    db.store.pool().close().await;
    let request = interaction(110, reason_options(), MANAGE_CHANNELS);
    assert!(
        runtime.execute(&router(true), &request).await.is_err(),
        "a dead store is an error, never a fabricated preview"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn preview_redelivered_interaction_refuses_without_second_row() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let router = router(true);
    let first = interaction(111, reason_options(), MANAGE_CHANNELS);
    let reply = handled(
        runtime.execute(&router, &first).await.unwrap(),
        "lockdown-preview",
    );
    assert_eq!(reply.outcome, "lockdown_preview");
    assert_eq!(db.audit_count().await, 1);
    // Same interaction id redelivered: same reads, zero new audit rows, and
    // the pending entry the first attempt armed is unwound.
    let second = interaction(111, reason_options(), MANAGE_CHANNELS);
    let reply = handled(
        runtime.execute(&router, &second).await.unwrap(),
        "lockdown-preview",
    );
    assert_eq!(reply.outcome, "refused");
    assert!(reply.digest32.is_none(), "redelivery renders nothing");
    assert_eq!(db.audit_count().await, 1, "redelivery writes no second row");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn preview_rate_budget_refuses_without_reads_or_rows() {
    let db = Database::open().await;
    let mut script = Vec::new();
    for _ in 0..10 {
        script.push(ScriptedResponse::json(200, armed_channel_doc()));
        script.push(ScriptedResponse::json(200, roles_doc()));
    }
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    let router = router(true);
    for id in 120..130 {
        let request = interaction(id, reason_options(), MANAGE_CHANNELS);
        let reply = handled(
            runtime.execute(&router, &request).await.unwrap(),
            "lockdown-preview",
        );
        assert_eq!(reply.outcome, "lockdown_preview", "budget admits 10");
    }
    assert_eq!(db.audit_count().await, 10);
    assert_eq!(
        mock.requests().iter().filter(|r| r.method == "GET").count(),
        20,
        "each admitted preview costs one channel GET plus one roles GET"
    );
    let request = interaction(130, reason_options(), MANAGE_CHANNELS);
    let reply = handled(
        runtime.execute(&router, &request).await.unwrap(),
        "lockdown-preview",
    );
    assert_eq!(reply.outcome, "refused");
    assert!(
        contains_ci(&reply.text, "rate limit"),
        "budget refusal names the rate limit: {}",
        reply.text
    );
    assert_eq!(
        mock.requests().iter().filter(|r| r.method == "GET").count(),
        20,
        "the refused attempt reads nothing"
    );
    assert_eq!(
        db.audit_count().await,
        10,
        "the refused attempt audits nothing"
    );
    mock.shutdown().await;
    db.close().await;
}
