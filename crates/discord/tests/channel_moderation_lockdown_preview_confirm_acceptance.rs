//! Slice 5 acceptance: `/lockdown-preview` Confirm/Cancel through the
//! existing lockdown mutation path.
//!
//! Contract under test (spec sections 3, 6, 8; matrix LP-08…LP-11, LP-14,
//! LP-16):
//!
//! * Confirm consumes the pending entry once, re-runs router adjudication
//!   before any read, re-reads the three live inputs, and refuses as stale
//!   on any digest mismatch with exactly one `{"stale_preview":1}` row and
//!   zero claim/lane/seed/PUT. A change confined to non-`INTEREST` bits
//!   executes.
//! * An exact match enters the existing mutation path with the pending reason
//!   and the confirm interaction id as idempotency identity: one lockdown
//!   row whose `preview_request_id` equals the confirmed preview's
//!   interaction id, zero extra rows, and the lane's own recheck refusing
//!   before any seed or PUT on drift.
//! * Actor-gate loss refuses before reads with the entry already consumed
//!   and no row; foreign/forged/double/cancel-then-confirm/restart presses
//!   collapse to `PREVIEW_EXPIRED` with no row. Cancel consumes the entry,
//!   collapses the buttons, and writes no row.
//! * A lane held by another action answers uncertain with the entry
//!   consumed and no PUT; a refused PUT finishes as no-effect with its seed
//!   retired.
//! * Transport: an owned live press defers with type 6 and clears the
//!   buttons; any other press defers ephemeral (type 5) and edits text-only,
//!   preserving live buttons.
//!
//! Expiry-at-boundary (`now == expires`) is covered at the store seam by the
//! `expiry_equality_frees_and_take_refuses_expired` unit test: driving a
//! 300-second TTL end to end is not feasible in-test, and the confirm path
//! consumes through that same `take`.

#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::Row;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{ChannelClaim, ChannelModerationStore, InteractionRouter, RouterGates};
use two_bot_discord::{
    lockdown_preview::{cancel_custom_id, confirm_custom_id, PREVIEW_EXPIRED},
    register_channel_handlers, ActionExecutor, ChannelModerationRuntime, ChannelReply,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "111111111111111111";
const CHANNEL: &str = "222222222222222222";
const ACTOR: &str = "333333333333333333";
const ACTOR_U64: u64 = 333333333333333333;
const OTHER: &str = "555555555555555555";
const KEEPER_ROLE: &str = "444444444444444444";
const MANAGE_CHANNELS: u64 = 1 << 4;

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

    async fn audit_rows(&self) -> Vec<(String, String, String, String)> {
        sqlx::query(
            "SELECT request_id, outcome, reason, metadata_json FROM moderation_audit ORDER BY id",
        )
        .fetch_all(self.store.pool())
        .await
        .unwrap()
        .iter()
        .map(|row| {
            (
                row.get::<String, _>("request_id"),
                row.get::<String, _>("outcome"),
                row.get::<String, _>("reason"),
                row.get::<String, _>("metadata_json"),
            )
        })
        .collect()
    }

    async fn lockdown_present(&self) -> bool {
        self.store.get_lockdown(CHANNEL).await.unwrap().is_some()
    }

    async fn close(self) {
        self.fixture.close().await.expect("verify fixture teardown");
    }
}

fn router() -> InteractionRouter {
    let mut router = InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD.parse().unwrap()),
        moderation: true,
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

fn slash(id: u64, permissions: u64) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "444444444444444444", "type": 2,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0, "permissions": permissions.to_string()},
        "data": {"id": "555555555555555555", "type": 1, "name": "lockdown-preview",
                 "options": [{"name": "reason", "type": 3, "value": "raid drill"}]},
        "member": {"user": {"id": ACTOR, "username": "test-member", "discriminator": "0", "avatar": null},
                   "roles": [], "flags": 0, "deaf": false, "mute": false, "permissions": permissions.to_string()},
        "token": "synthetic-interaction-token", "version": 1, "locale": "en-US", "guild_locale": "en-US",
        "authorizing_integration_owners": {"0": GUILD}, "entitlements": []
    })).expect("synthetic Twilight interaction")
}

fn press(id: u64, user: &str, permissions: u64, custom_id: &str) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "444444444444444444", "type": 3,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0, "permissions": permissions.to_string()},
        "member": {"user": {"id": user, "username": "member", "discriminator": "0", "avatar": null},
                   "roles": [], "flags": 0, "deaf": false, "mute": false, "permissions": permissions.to_string()},
        "token": "synthetic-interaction-token", "version": 1, "locale": "en-US", "guild_locale": "en-US",
        "authorizing_integration_owners": {"0": GUILD}, "entitlements": [],
        "data": {"custom_id": custom_id, "component_type": 2}
    })).expect("synthetic Twilight component interaction")
}

fn runtime(db: &Database, mock: &MockRest) -> ChannelModerationRuntime {
    ChannelModerationRuntime::new(
        db.store.clone(),
        ActionExecutor::with_proxy("synthetic-test-token".to_owned(), Some(mock.origin())).unwrap(),
    )
}

fn handled(reply: Option<ChannelReply>, command: &str) -> ChannelReply {
    reply.unwrap_or_else(|| panic!("{command} is not handled by ChannelModerationRuntime"))
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

/// `@everyone` also denies `SEND_MESSAGES` (an `INTEREST` lock bit): any
/// preview armed on the doc above is stale against this state.
fn stale_channel_doc() -> Value {
    json!({
        "id": CHANNEL,
        "type": 0,
        "guild_id": GUILD,
        "permission_overwrites": [{"id": GUILD, "type": 0, "allow": "1024", "deny": "2048"}],
    })
}

fn roles_doc() -> Value {
    json!([
        {"id": GUILD, "position": 0, "permissions": "104324673"},
        {"id": KEEPER_ROLE, "position": 1, "permissions": "3072"},
    ])
}

/// Keeper role with `EMBED_LINKS` added: outside `INTEREST_BITS`, so the
/// digest is unchanged and a confirm still executes (LP-09).
fn roles_doc_embed() -> Value {
    json!([
        {"id": GUILD, "position": 0, "permissions": "104324673"},
        {"id": KEEPER_ROLE, "position": 1, "permissions": "19456"},
    ])
}

fn patched() -> ScriptedResponse {
    ScriptedResponse::json(200, json!({"id": CHANNEL}))
}

async fn preview_armed(runtime: &ChannelModerationRuntime, id: u64) -> String {
    let reply = handled(
        runtime
            .execute(&router(), &slash(id, MANAGE_CHANNELS))
            .await
            .unwrap(),
        "lockdown-preview",
    );
    assert_eq!(reply.outcome, "lockdown_preview");
    reply.digest32.expect("armed previews carry a digest")
}

fn put_count(mock: &MockRest) -> usize {
    mock.requests().iter().filter(|r| r.method == "PUT").count()
}

fn all_get(mock: &MockRest) {
    assert!(
        mock.requests().iter().all(|r| r.method == "GET"),
        "no mutation attempted: {:?}",
        mock.requests()
            .iter()
            .map(|r| (r.method.clone(), r.path.clone()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn component_presses_route_to_the_channel_slice() {
    let digest = "ab12cd34ef56ab12cd34ef56ab12cd34";
    assert!(ChannelModerationRuntime::accepts(&press(
        1,
        ACTOR,
        MANAGE_CHANNELS,
        &confirm_custom_id(ACTOR_U64, digest)
    )));
    assert!(ChannelModerationRuntime::accepts(&press(
        2,
        ACTOR,
        MANAGE_CHANNELS,
        &cancel_custom_id(ACTOR_U64, digest)
    )));
    // A foreign prefix is not ours: the slice ignores it.
    assert!(!ChannelModerationRuntime::accepts(&press(
        3,
        ACTOR,
        MANAGE_CHANNELS,
        "gw:123"
    )));
}

#[tokio::test]
async fn confirm_executes_once_then_the_second_press_expires() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let digest = preview_armed(&runtime, 200).await;

    let confirm = press(
        201,
        ACTOR,
        MANAGE_CHANNELS,
        &confirm_custom_id(ACTOR_U64, &digest),
    );
    let reply = handled(
        runtime.execute(&router(), &confirm).await.unwrap(),
        "confirm",
    );
    assert_eq!(reply.outcome, "locked_down");
    assert!(db.lockdown_present().await, "confirm seeds the lockdown");

    // One preview row plus one execution row carrying the correlation.
    let rows = db.audit_rows().await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "200");
    assert_eq!(rows[0].1, "lockdown_preview");
    assert_eq!(rows[1].0, "201");
    assert_eq!(rows[1].1, "locked_down");
    assert_eq!(
        rows[1].2, "raid drill",
        "the pending reason flows into the execution row"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&rows[1].3).unwrap(),
        json!({"preview_request_id": "200"}),
        "the confirmed preview's interaction id correlates the execution row"
    );

    // The entry is consumed: a double press expires with no third row.
    let replay = handled(
        runtime
            .execute(
                &router(),
                &press(
                    202,
                    ACTOR,
                    MANAGE_CHANNELS,
                    &confirm_custom_id(ACTOR_U64, &digest),
                ),
            )
            .await
            .unwrap(),
        "double confirm",
    );
    assert_eq!(replay.outcome, PREVIEW_EXPIRED);
    assert_eq!(db.audit_count().await, 2, "the double press writes no row");
    assert_eq!(
        put_count(&mock),
        1,
        "exactly one PUT across preview and confirm"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn confirm_stale_on_lock_bit_change_writes_one_row_without_mutation() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
            ScriptedResponse::json(200, stale_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let digest = preview_armed(&runtime, 210).await;

    let reply = handled(
        runtime
            .execute(
                &router(),
                &press(
                    211,
                    ACTOR,
                    MANAGE_CHANNELS,
                    &confirm_custom_id(ACTOR_U64, &digest),
                ),
            )
            .await
            .unwrap(),
        "stale confirm",
    );
    assert_eq!(reply.outcome, "refused");
    assert!(
        reply.text.contains("changed after this preview"),
        "stale text names the divergence: {}",
        reply.text
    );
    let rows = db.audit_rows().await;
    assert_eq!(rows.len(), 2, "preview row plus exactly one stale row");
    assert_eq!(rows[1].0, "211");
    assert_eq!(rows[1].1, "refused");
    assert_eq!(
        serde_json::from_str::<Value>(&rows[1].3).unwrap(),
        json!({"stale_preview": 1}),
        "the stale row pins the divergence with the fixed marker"
    );
    assert!(
        !db.lockdown_present().await,
        "a stale confirm seeds nothing"
    );
    all_get(&mock);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn confirm_executes_when_only_non_interest_bits_change() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc_embed()),
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let digest = preview_armed(&runtime, 220).await;

    let reply = handled(
        runtime
            .execute(
                &router(),
                &press(
                    221,
                    ACTOR,
                    MANAGE_CHANNELS,
                    &confirm_custom_id(ACTOR_U64, &digest),
                ),
            )
            .await
            .unwrap(),
        "non-interest confirm",
    );
    assert_eq!(
        reply.outcome, "locked_down",
        "an EMBED_LINKS-only role change leaves the digest intact"
    );
    assert_eq!(db.audit_count().await, 2);
    assert_eq!(put_count(&mock), 1);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn confirm_refused_when_actor_loses_manage_channels_consumes_without_reads_or_rows() {
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
    let digest = preview_armed(&runtime, 230).await;

    // The gate closed after the preview: refused before any read, no row, but
    // the entry is already consumed.
    let reply = handled(
        runtime
            .execute(
                &router(),
                &press(231, ACTOR, 0, &confirm_custom_id(ACTOR_U64, &digest)),
            )
            .await
            .unwrap(),
        "demoted confirm",
    );
    assert_eq!(reply.outcome, "refused");
    assert!(reply.text.contains("Nothing was changed"), "{}", reply.text);
    assert_eq!(db.audit_count().await, 1, "router refusals write no row");
    assert_eq!(
        mock.requests().len(),
        2,
        "the refused confirm reads nothing"
    );

    // The entry stays consumed even for a re-empowered press.
    let retry = handled(
        runtime
            .execute(
                &router(),
                &press(
                    232,
                    ACTOR,
                    MANAGE_CHANNELS,
                    &confirm_custom_id(ACTOR_U64, &digest),
                ),
            )
            .await
            .unwrap(),
        "retry confirm",
    );
    assert_eq!(retry.outcome, PREVIEW_EXPIRED);
    assert_eq!(db.audit_count().await, 1);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn foreign_forged_cancel_and_restart_presses_expire_without_rows() {
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
    let digest = preview_armed(&runtime, 240).await;
    let router = router();

    // A foreign actor naming the owner's entry expires without touching it.
    let foreign = handled(
        runtime
            .execute(
                &router,
                &press(
                    241,
                    OTHER,
                    MANAGE_CHANNELS,
                    &confirm_custom_id(ACTOR_U64, &digest),
                ),
            )
            .await
            .unwrap(),
        "foreign confirm",
    );
    assert_eq!(foreign.outcome, PREVIEW_EXPIRED);

    // A forged digest by the owner expires the same way.
    let forged = handled(
        runtime
            .execute(
                &router,
                &press(
                    242,
                    ACTOR,
                    MANAGE_CHANNELS,
                    &confirm_custom_id(ACTOR_U64, &"00".repeat(16)),
                ),
            )
            .await
            .unwrap(),
        "forged confirm",
    );
    assert_eq!(forged.outcome, PREVIEW_EXPIRED);

    // Cancel consumes the entry with no audit row.
    let cancel = handled(
        runtime
            .execute(
                &router,
                &press(
                    243,
                    ACTOR,
                    MANAGE_CHANNELS,
                    &cancel_custom_id(ACTOR_U64, &digest),
                ),
            )
            .await
            .unwrap(),
        "cancel",
    );
    assert_eq!(cancel.outcome, "cancelled");
    assert!(
        cancel.text.contains("Nothing was changed"),
        "cancel admits no mutation: {}",
        cancel.text
    );
    assert_eq!(db.audit_count().await, 1, "cancel writes no row");

    // Cancel-then-confirm finds no entry.
    let after_cancel = handled(
        runtime
            .execute(
                &router,
                &press(
                    244,
                    ACTOR,
                    MANAGE_CHANNELS,
                    &confirm_custom_id(ACTOR_U64, &digest),
                ),
            )
            .await
            .unwrap(),
        "confirm after cancel",
    );
    assert_eq!(after_cancel.outcome, PREVIEW_EXPIRED);

    // A restart drops the in-memory pending set: the same buttons expire.
    let runtime2 = runtime(&db, &mock);
    let after_restart = handled(
        runtime2
            .execute(
                &router,
                &press(
                    245,
                    ACTOR,
                    MANAGE_CHANNELS,
                    &confirm_custom_id(ACTOR_U64, &digest),
                ),
            )
            .await
            .unwrap(),
        "confirm after restart",
    );
    assert_eq!(after_restart.outcome, PREVIEW_EXPIRED);
    assert_eq!(db.audit_count().await, 1, "no press wrote a row");
    all_get(&mock);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn prepare_lane_drift_refuses_before_seed_or_put_with_correlation() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
            // The confirm recheck still matches, but the lane's own GET drifts.
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
            ScriptedResponse::json(200, stale_channel_doc()),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let digest = preview_armed(&runtime, 250).await;

    let reply = handled(
        runtime
            .execute(
                &router(),
                &press(
                    251,
                    ACTOR,
                    MANAGE_CHANNELS,
                    &confirm_custom_id(ACTOR_U64, &digest),
                ),
            )
            .await
            .unwrap(),
        "drifted confirm",
    );
    assert_eq!(reply.outcome, "refused");
    assert!(
        reply.text.contains("changed after this preview"),
        "{}",
        reply.text
    );
    let rows = db.audit_rows().await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].0, "251");
    assert_eq!(
        serde_json::from_str::<Value>(&rows[1].3).unwrap(),
        json!({"preview_request_id": "250"}),
        "a mutation-path refusal keeps the preview correlation"
    );
    assert!(
        !db.lockdown_present().await,
        "drift refuses before any seed"
    );
    all_get(&mock);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn put_403_finishes_as_no_effect_refusal_with_seed_retired() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::status(403),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let digest = preview_armed(&runtime, 260).await;

    let reply = handled(
        runtime
            .execute(
                &router(),
                &press(
                    261,
                    ACTOR,
                    MANAGE_CHANNELS,
                    &confirm_custom_id(ACTOR_U64, &digest),
                ),
            )
            .await
            .unwrap(),
        "refused-put confirm",
    );
    assert_eq!(reply.outcome, "refused");
    assert!(
        reply.text.contains("no mutation was accepted"),
        "{}",
        reply.text
    );
    assert!(
        !db.lockdown_present().await,
        "a refused first PUT retires its seed"
    );
    assert_eq!(db.audit_count().await, 2);
    assert_eq!(
        put_count(&mock),
        1,
        "the single refused PUT is the only write"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn lane_held_confirm_is_uncertain_consumed_and_put_free() {
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
    // Another in-flight channel action holds this channel's lane.
    let at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("UTC formats");
    let ticket = match db
        .store
        .claim(
            GUILD,
            "lane-holder-test",
            "moderation.lockdown",
            "hash",
            &at,
        )
        .await
        .unwrap()
    {
        ChannelClaim::Claimed { ticket } => ticket,
        _ => panic!("the holder claim wins an empty lane"),
    };
    assert!(db.store.claim_channel(&ticket, CHANNEL).await.unwrap());
    let runtime = runtime(&db, &mock);
    let digest = preview_armed(&runtime, 270).await;

    let reply = handled(
        runtime
            .execute(
                &router(),
                &press(
                    271,
                    ACTOR,
                    MANAGE_CHANNELS,
                    &confirm_custom_id(ACTOR_U64, &digest),
                ),
            )
            .await
            .unwrap(),
        "lane-held confirm",
    );
    assert_eq!(reply.outcome, "in_progress");
    let rows = db.audit_rows().await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].1, "in_progress");
    all_get(&mock);

    // The entry is consumed: releasing the lane does not revive the press.
    db.store.release(&ticket).await.unwrap();
    let retry = handled(
        runtime
            .execute(
                &router(),
                &press(
                    272,
                    ACTOR,
                    MANAGE_CHANNELS,
                    &confirm_custom_id(ACTOR_U64, &digest),
                ),
            )
            .await
            .unwrap(),
        "confirm after lane release",
    );
    assert_eq!(retry.outcome, PREVIEW_EXPIRED);
    assert_eq!(db.audit_count().await, 2);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn respond_confirm_collapses_buttons_with_deferred_update() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::status(204),
            patched(),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let router = router();
    let digest = preview_armed(&runtime, 280).await;

    assert!(
        runtime
            .respond(
                &router,
                &press(
                    281,
                    ACTOR,
                    MANAGE_CHANNELS,
                    &confirm_custom_id(ACTOR_U64, &digest)
                )
            )
            .await
            .unwrap(),
        "an owned live press completes the collapse lifecycle"
    );
    let requests = mock.requests();
    assert_eq!(requests[2].method, "POST");
    assert!(requests[2].path.ends_with("/callback"));
    let defer: Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(
        defer["type"], 6,
        "collapse defers with an update to the button message"
    );
    assert!(
        defer.get("data").is_none(),
        "a deferred update carries no message data"
    );
    let edits: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
    assert_eq!(edits.len(), 1);
    let edit: Value = serde_json::from_slice(&edits[0].body).unwrap();
    assert!(
        edit["content"].as_str().unwrap().contains("locked_down"),
        "{edit}"
    );
    assert_eq!(
        edit["components"],
        json!([]),
        "the completion edit clears the consumed buttons"
    );
    assert_eq!(put_count(&mock), 1);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn respond_foreign_press_stays_ephemeral_and_preserves_live_buttons() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
            ScriptedResponse::status(204),
            patched(),
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::json(200, roles_doc()),
            ScriptedResponse::json(200, armed_channel_doc()),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let router = router();
    let digest = preview_armed(&runtime, 290).await;

    // A foreign press answers with a fresh ephemeral notice and leaves the
    // live entry — and its buttons — alone.
    assert!(runtime
        .respond(
            &router,
            &press(
                291,
                OTHER,
                MANAGE_CHANNELS,
                &confirm_custom_id(ACTOR_U64, &digest)
            )
        )
        .await
        .unwrap());
    let requests = mock.requests();
    assert_eq!(requests[2].method, "POST");
    let defer: Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(defer["type"], 5);
    assert_eq!(defer["data"]["flags"], 64);
    let edits: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
    assert_eq!(edits.len(), 1);
    let edit: Value = serde_json::from_slice(&edits[0].body).unwrap();
    assert!(
        edit.get("components").is_none(),
        "a foreign-press edit must not clear live buttons: {edit}"
    );

    // The owner's entry survived: the real Confirm still executes.
    let reply = handled(
        runtime
            .execute(
                &router,
                &press(
                    292,
                    ACTOR,
                    MANAGE_CHANNELS,
                    &confirm_custom_id(ACTOR_U64, &digest),
                ),
            )
            .await
            .unwrap(),
        "owner confirm after foreign press",
    );
    assert_eq!(reply.outcome, "locked_down");
    assert_eq!(db.audit_count().await, 2, "the foreign press wrote no row");
    assert_eq!(put_count(&mock), 1);
    mock.shutdown().await;
    db.close().await;
}
