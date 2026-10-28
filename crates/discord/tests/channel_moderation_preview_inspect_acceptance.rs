#![cfg(feature = "db")]

//! Protected RED acceptance: staff-gated purge preview + channel-restriction inspect.
//!
//! The implementation builds on this commit and must preserve these acceptance
//! assertions. Report an incorrect test to its author instead of working around it.
//!
//! Normative contract under test (the Builder implements exactly this):
//!
//! * Two new private staff reads on the channel slice, served by
//!   [`ChannelModerationRuntime`] through the shared [`InteractionRouter`]
//!   (with [`register_channel_handlers`]) and the ephemeral [`respond`]
//!   lifecycle:
//!   - `purge-preview` with one required integer option `count` (1-100, the
//!     same bound and the same `ManageMessages` gate as `/purge`). No `reason`
//!     option: reads are not audited, so a mandatory audit reason would be
//!     wrong UX.
//!   - `channel-inspect` with no options, gated on `ManageChannels` (the
//!     `/lockdown` + `/slowmode` gate).
//! * Outcomes: `purge_preview` and `channel_inspect` on success; `refused` on
//!   every refusal and on unrecoverable read failures (history/store), always
//!   with private retry guidance. Absent state renders as none/off, never as
//!   an error.
//! * Pure reads: NO delete, NO lockdown/slowmode/config change, NO audit row,
//!   NO idempotency claim, NO channel-lane write on ANY path, including
//!   refusals. This deliberately deviates from the mutating verbs (which audit
//!   refusals): a read probe must not spam the audit ledger.
//! * Preview text tokens (exact, lowercase): `targetable: N` (listed minus
//!   pinned minus own-bot posts), `span: <first>..<last>` over the targetable
//!   set in listed order (`span: none` when empty), `pinned: P`, `bot: B`
//!   (own-bot posts skipped), `system: S` (listed messages whose `type` is
//!   neither 0 nor 19 nor missing), `bulk: K, single: M` (the [`plan_purge`]
//!   split: recent and at least two go bulk, everything else single), and the
//!   race disclaimer `not a guarantee`. Message bodies are NEVER echoed.
//! * Inspect text tokens (exact, lowercase): `lockdown: active` /
//!   `lockdown: none` (from the existing store getter), `reason: <stored>`
//!   verbatim when active, `set at: YYYY-MM-DD` (calendar date from the stored
//!   `locked_at`) or `set at: unknown`, `set by: <actor id>` (latest
//!   `moderation.lockdown` audit row for the channel) or `set by: unknown`,
//!   and `slowmode: <n>s` / `slowmode: off` (0 or missing) /
//!   `slowmode: unknown` (live channel read failed; the inspect still succeeds
//!   with retry guidance).
//! * Preview follows the purge list-path read order: one history read, then a
//!   bot-identity read only when some listed author is not explicitly human.
//!   The inspect may read the live channel once or twice (whole-document
//!   fetch, or overwrite read plus document fetch).
//! * Failures: history-API failure -> `refused` + case-insensitive "try
//!   again"; store outage -> `refused` + "try again" (never `Err`, never a
//!   panic); live channel-read failure -> `channel_inspect` with
//!   `slowmode: unknown` + "try again" (partial success, store half intact).
//! * Staff-authored lockdown reasons echo verbatim (bounded, 512 chars) with
//!   `allowed_mentions.parse == []` so hostile mentions render inert; hostile
//!   MESSAGE content never appears in any reply.
//!
//! [`respond`]: ChannelModerationRuntime::respond
//! [`plan_purge`]: two_bot_core::plan_purge

#[allow(dead_code)]
mod common;

use common::{aged_message_id, fresh_message_id, MockRest, ScriptedResponse};
use serde_json::{json, Value};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{ChannelModerationStore, InteractionRouter, RouterGates};
use two_bot_discord::{
    register_channel_handlers, ActionExecutor, ChannelModerationRuntime, ChannelReply,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "111111111111111111";
const CHANNEL: &str = "222222222222222222";
const ACTOR: &str = "333333333333333333";
const BOT_USER_ID: &str = "777777777777777777";
const PERMISSIONS: u64 = (1 << 4) | (1 << 13);
const MANAGE_CHANNELS_ONLY: u64 = 1 << 4;
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

    async fn count(&self, table: &str) -> i64 {
        assert!([
            "moderation_audit",
            "moderation_idempotency",
            "moderation_channel_executions",
            "moderation_lockdowns"
        ]
        .contains(&table));
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(self.store.pool())
            .await
            .unwrap()
    }

    /// (audit, idempotency, executions, lockdowns) for before/after comparison.
    async fn snapshot(&self) -> (i64, i64, i64, i64) {
        (
            self.count("moderation_audit").await,
            self.count("moderation_idempotency").await,
            self.count("moderation_channel_executions").await,
            self.count("moderation_lockdowns").await,
        )
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

fn interaction(id: u64, action: &str, options: Value, permissions: u64) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "444444444444444444", "type": 2,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0, "permissions": permissions.to_string()},
        "data": {"id": "555555555555555555", "type": 1, "name": action, "options": options},
        "member": {"user": {"id": ACTOR, "username": "test-member", "discriminator": "0", "avatar": null},
                   "roles": [], "flags": 0, "deaf": false, "mute": false, "permissions": permissions.to_string()},
        "token": "synthetic-interaction-token", "version": 1, "locale": "en-US", "guild_locale": "en-US",
        "authorizing_integration_owners": {"0": GUILD}, "entitlements": []
    })).expect("synthetic Twilight interaction")
}

/// Preview carries only `count`: reads take no audit reason.
fn preview_options(count: i64) -> Value {
    json!([{"name": "count", "type": 4, "value": count}])
}

fn inspect_options() -> Value {
    json!([])
}

/// Mutating-verb options (with the mandatory reason) for setup flows only.
fn reason_options(numeric: Option<(&str, i64)>) -> Value {
    let mut options = vec![json!({"name": "reason", "type": 3, "value": "mock audit reason"})];
    if let Some((name, number)) = numeric {
        options.push(json!({"name": name, "type": 4, "value": number}));
    }
    json!(options)
}

fn runtime(db: &Database, mock: &MockRest) -> ChannelModerationRuntime {
    ChannelModerationRuntime::new(
        db.store.clone(),
        ActionExecutor::with_proxy("synthetic-test-token".to_owned(), Some(mock.origin())).unwrap(),
    )
}

/// Unwrap a handled reply, or fail RED with the missing command's name.
/// Every test below must fail here until the reads are implemented: a `None`
/// means the runtime ignored the command entirely.
fn handled(reply: Option<ChannelReply>, command: &str) -> ChannelReply {
    reply.unwrap_or_else(|| {
        panic!("{command} is not handled by ChannelModerationRuntime yet (RED: unimplemented read)")
    })
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// No HTTP at all happened (validation/router refusals).
fn assert_no_requests(mock: &MockRest) {
    assert!(
        mock.requests().is_empty(),
        "a refused read must not touch Discord: {:?}",
        mock.requests()
            .iter()
            .map(|r| (r.method.clone(), r.path.clone()))
            .collect::<Vec<_>>()
    );
}

/// Every Discord call was a GET: no bulk/single delete, no overwrite PUT,
/// no slowmode PATCH, no callback POST (execute-level, no defer).
fn assert_get_only(mock: &MockRest) {
    for request in mock.requests() {
        assert_eq!(
            request.method, "GET",
            "reads never mutate over HTTP: {} {}",
            request.method, request.path
        );
    }
}

/// Respond-level: the only non-GETs are the defer callback POST and the
/// `@original` PATCH. Nothing else may mutate.
fn assert_no_channel_writes(mock: &MockRest) {
    for request in mock.requests() {
        if request.method == "GET" {
            continue;
        }
        let callback = request.method == "POST"
            && request.path.starts_with("/api/v10/interactions/")
            && request.path.ends_with("/callback");
        let edit = request.method == "PATCH"
            && request.path.starts_with("/api/v10/webhooks/")
            && request.path.ends_with("/messages/@original");
        assert!(
            callback || edit,
            "non-GET outside the interaction reply lifecycle is a mutation: {} {}",
            request.method,
            request.path
        );
    }
    assert!(
        !mock
            .requests()
            .iter()
            .any(|r| r.path.contains("bulk-delete") || r.method == "DELETE"),
        "reads never delete"
    );
}

fn assert_history_read(mock: &MockRest) {
    assert!(
        mock.requests()
            .iter()
            .any(|r| r.method == "GET" && r.path.contains("/messages")),
        "preview reads history: {:?}",
        mock.requests()
            .iter()
            .map(|r| (r.method.clone(), r.path.clone()))
            .collect::<Vec<_>>()
    );
}

fn assert_channel_read(mock: &MockRest) {
    assert!(
        mock.requests()
            .iter()
            .any(|r| r.method == "GET" && r.path == format!("/api/v10/channels/{CHANNEL}")),
        "inspect reads the live channel: {:?}",
        mock.requests()
            .iter()
            .map(|r| (r.method.clone(), r.path.clone()))
            .collect::<Vec<_>>()
    );
}

/// The inspect issued its own live channel read after `before` (setup flows
/// also GET the channel, so the whole-mock check would pass vacuously).
fn assert_new_channel_read(mock: &MockRest, before: usize) {
    assert!(
        mock.requests()[before..]
            .iter()
            .any(|r| r.method == "GET" && r.path == format!("/api/v10/channels/{CHANNEL}")),
        "inspect reads the live channel itself: {:?}",
        mock.requests()
            .iter()
            .map(|r| (r.method.clone(), r.path.clone()))
            .collect::<Vec<_>>()
    );
}

async fn assert_all_zero(db: &Database) {
    assert_eq!(db.snapshot().await, (0, 0, 0, 0));
}

/// One history row. `msg_type=None` omits `type` (counts as a normal message).
fn msg(
    id: String,
    msg_type: Option<u64>,
    pinned: bool,
    author_id: &str,
    author_bot: Option<bool>,
    content: &str,
) -> Value {
    let mut author = json!({"id": author_id});
    if let Some(bot) = author_bot {
        author["bot"] = json!(bot);
    }
    let mut row = json!({
        "id": id,
        "pinned": pinned,
        "author": author,
        "content": content,
    });
    if let Some(kind) = msg_type {
        row["type"] = json!(kind);
    }
    row
}

/// Full live channel document: id/type/guild for identity checks,
/// `rate_limit_per_user` for slowmode, overwrites for @everyone reads.
fn channel_doc(rate_limit_per_user: Option<u64>, locked: bool) -> Value {
    let deny = if locked { "10240" } else { "8192" };
    let mut doc = json!({
        "id": CHANNEL,
        "type": 0,
        "guild_id": GUILD,
        "permission_overwrites": [{"id": GUILD, "type": 0, "allow": "1024", "deny": deny}],
    });
    if let Some(seconds) = rate_limit_per_user {
        doc["rate_limit_per_user"] = json!(seconds);
    }
    doc
}

fn overwrite(allow: &str, deny: &str) -> ScriptedResponse {
    ScriptedResponse::json(
        200,
        json!({"permission_overwrites": [{"id": GUILD, "type": 0, "allow": allow, "deny": deny}]}),
    )
}

/// Two servable channel documents: the inspect may read the live channel once
/// (whole-document fetch) or twice (overwrite read + document fetch). A third
/// read hits the 500 default and must surface as unknown, never fabricated.
fn doc_script(rate_limit_per_user: Option<u64>, locked: bool) -> Vec<ScriptedResponse> {
    vec![
        ScriptedResponse::json(200, channel_doc(rate_limit_per_user, locked)),
        ScriptedResponse::json(200, channel_doc(rate_limit_per_user, locked)),
    ]
}

async fn seed_lockdown(db: &Database, reason: &str, locked_at: &str) {
    db.store
        .record_lockdown(
            CHANNEL,
            GUILD,
            &two_bot_core::LockdownSeed {
                prior_allow: "1024".to_owned(),
                prior_deny: "8192".to_owned(),
                prior_exists: true,
            },
            reason,
            locked_at,
        )
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// Command surface: the router/slice must accept the two new reads.
// These run without a database, so RED is visible in a plain `cargo test`.
// ---------------------------------------------------------------------------

#[test]
fn accepts_purge_preview_command() {
    let request = interaction(1, "purge-preview", preview_options(10), PERMISSIONS);
    assert!(
        ChannelModerationRuntime::accepts(&request),
        "RED: purge-preview is not routed to the channel slice yet"
    );
}

#[test]
fn accepts_channel_inspect_command() {
    let request = interaction(2, "channel-inspect", inspect_options(), PERMISSIONS);
    assert!(
        ChannelModerationRuntime::accepts(&request),
        "RED: channel-inspect is not routed to the channel slice yet"
    );
}

// ---------------------------------------------------------------------------
// purge-preview
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preview_counts_match_deletable_set_with_pin_and_age_exclusions() {
    let db = Database::open().await;
    let (fresh_a, fresh_b, fresh_c) = (
        fresh_message_id(1),
        fresh_message_id(2),
        fresh_message_id(3),
    );
    let (old_a, old_b) = (aged_message_id(4), aged_message_id(5));
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            json!([
                msg(fresh_a.clone(), Some(0), false, "10", Some(false), "hello"),
                msg(
                    fresh_c.clone(),
                    Some(0),
                    true,
                    "11",
                    Some(false),
                    "pinned note"
                ),
                msg(fresh_b.clone(), Some(0), false, "12", Some(false), "world"),
                msg(old_a.clone(), Some(0), false, "13", Some(false), "old one"),
                msg(
                    old_b.clone(),
                    Some(0),
                    false,
                    "14",
                    Some(false),
                    "older two"
                ),
            ]),
        )],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    // count=100 over 5 listed: the bound is an upper limit, not a requirement.
    let request = interaction(100, "purge-preview", preview_options(100), PERMISSIONS);
    let reply = handled(
        runtime.execute(&router(true), &request).await.unwrap(),
        "purge-preview",
    );
    assert_eq!(reply.outcome, "purge_preview");
    assert!(
        reply.text.contains("targetable: 4"),
        "pinned is excluded, the rest targets: {}",
        reply.text
    );
    assert!(
        reply.text.contains("pinned: 1"),
        "pinned exclusion is reported: {}",
        reply.text
    );
    assert!(
        reply.text.contains("bulk: 2, single: 2"),
        "recent pair goes bulk, aged pair goes singly (plan_purge rule): {}",
        reply.text
    );
    assert!(
        reply.text.contains(&format!("span: {fresh_a}..{old_b}")),
        "span covers the targetable set in listed order: {}",
        reply.text
    );
    assert!(
        reply.text.contains("not a guarantee"),
        "preview admits the race honestly: {}",
        reply.text
    );
    assert!(reply.text.contains("preview"), "{}", reply.text);
    assert_history_read(&mock);
    assert_get_only(&mock);
    assert_all_zero(&db).await;
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn preview_excludes_own_bot_posts_and_counts_system_messages() {
    let db = Database::open().await;
    let (own, other_bot, system, reply_msg, plain) = (
        fresh_message_id(11),
        fresh_message_id(12),
        fresh_message_id(13),
        fresh_message_id(14),
        fresh_message_id(15),
    );
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(
                200,
                json!([
                    msg(
                        own.clone(),
                        Some(0),
                        false,
                        BOT_USER_ID,
                        Some(true),
                        "panel"
                    ),
                    msg(
                        other_bot.clone(),
                        Some(0),
                        false,
                        "888",
                        Some(true),
                        "other bot spam"
                    ),
                    msg(
                        system.clone(),
                        Some(7),
                        false,
                        "21",
                        Some(false),
                        "join notice"
                    ),
                    msg(
                        reply_msg.clone(),
                        Some(19),
                        false,
                        "22",
                        Some(false),
                        "a reply"
                    ),
                    msg(plain.clone(), None, false, "23", Some(false), "plain"),
                ]),
            ),
            ScriptedResponse::json(200, json!({"id": BOT_USER_ID, "bot": true})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let request = interaction(101, "purge-preview", preview_options(5), PERMISSIONS);
    let reply = handled(
        runtime.execute(&router(true), &request).await.unwrap(),
        "purge-preview",
    );
    assert_eq!(reply.outcome, "purge_preview");
    assert!(
        reply.text.contains("targetable: 4"),
        "only our own bot post is skipped (another bot's spam still purges): {}",
        reply.text
    );
    assert!(
        reply.text.contains("bot: 1"),
        "own-bot exclusion is reported: {}",
        reply.text
    );
    assert!(
        reply.text.contains("system: 1"),
        "the type-7 join notice is counted; type-19 replies and typeless rows are normal: {}",
        reply.text
    );
    assert!(
        reply.text.contains(&format!("span: {other_bot}..{plain}")),
        "span skips the excluded own-bot head: {}",
        reply.text
    );
    assert!(reply.text.contains("not a guarantee"), "{}", reply.text);
    assert!(
        mock.requests()
            .iter()
            .any(|r| r.path.ends_with("/users/@me")),
        "ambiguous authors resolve the bot identity: {:?}",
        mock.requests()
            .iter()
            .map(|r| r.path.clone())
            .collect::<Vec<_>>()
    );
    assert_get_only(&mock);
    assert_all_zero(&db).await;
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn preview_empty_channel_reports_zero_without_error() {
    let db = Database::open().await;
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!([]))],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let request = interaction(102, "purge-preview", preview_options(50), PERMISSIONS);
    let reply = handled(
        runtime.execute(&router(true), &request).await.unwrap(),
        "purge-preview",
    );
    assert_eq!(reply.outcome, "purge_preview");
    assert!(reply.text.contains("targetable: 0"), "{}", reply.text);
    assert!(reply.text.contains("span: none"), "{}", reply.text);
    assert!(reply.text.contains("not a guarantee"), "{}", reply.text);
    assert_history_read(&mock);
    assert_get_only(&mock);
    assert_all_zero(&db).await;
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn preview_lone_recent_message_plans_a_single_delete() {
    let db = Database::open().await;
    let only = fresh_message_id(21);
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            json!([msg(only.clone(), Some(0), false, "10", Some(false), "solo")]),
        )],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let request = interaction(103, "purge-preview", preview_options(1), PERMISSIONS);
    let reply = handled(
        runtime.execute(&router(true), &request).await.unwrap(),
        "purge-preview",
    );
    assert_eq!(reply.outcome, "purge_preview");
    assert!(reply.text.contains("targetable: 1"), "{}", reply.text);
    assert!(
        reply.text.contains("bulk: 0, single: 1"),
        "one message can never bulk-delete (plan_purge lone rule): {}",
        reply.text
    );
    assert!(
        reply.text.contains(&format!("span: {only}..{only}")),
        "{}",
        reply.text
    );
    assert_get_only(&mock);
    assert_all_zero(&db).await;
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn preview_history_failure_refuses_with_retry_and_zero_effects() {
    let db = Database::open().await;
    for (id, history) in [
        (104, ScriptedResponse::status(503)),
        (
            105,
            ScriptedResponse::json(200, json!({"not": "a message list"})),
        ),
    ] {
        let mock = MockRest::start(vec![history], ScriptedResponse::status(500)).await;
        let runtime = runtime(&db, &mock);
        let request = interaction(id, "purge-preview", preview_options(10), PERMISSIONS);
        let reply = handled(
            runtime.execute(&router(true), &request).await.unwrap(),
            "purge-preview",
        );
        assert_eq!(reply.outcome, "refused");
        assert!(
            contains_ci(&reply.text, "try again"),
            "history failure carries retry guidance: {}",
            reply.text
        );
        assert_get_only(&mock);
        assert_all_zero(&db).await;
        mock.shutdown().await;
    }
    db.close().await;
}

#[tokio::test]
async fn preview_rejects_out_of_range_counts_without_http_or_writes() {
    let db = Database::open().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    for (id, options) in [
        (106, preview_options(0)),
        (107, preview_options(101)),
        (108, json!([])),
    ] {
        let request = interaction(id, "purge-preview", options, PERMISSIONS);
        let reply = handled(
            runtime.execute(&router(true), &request).await.unwrap(),
            "purge-preview",
        );
        assert_eq!(reply.outcome, "refused");
        assert!(
            reply.text.contains("between 1 and 100") || reply.text.contains("\"count\""),
            "bounds refusal names the count rule: {}",
            reply.text
        );
    }
    assert_no_requests(&mock);
    assert_all_zero(&db).await;
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn preview_refuses_non_staff_without_http_or_writes() {
    let db = Database::open().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    // Nobody (0) and ManageChannels-only staff: preview needs ManageMessages.
    for (id, permissions) in [(109, 0), (110, MANAGE_CHANNELS_ONLY)] {
        let request = interaction(id, "purge-preview", preview_options(10), permissions);
        let reply = handled(
            runtime.execute(&router(true), &request).await.unwrap(),
            "purge-preview",
        );
        assert_eq!(reply.outcome, "refused");
    }
    assert_no_requests(&mock);
    assert_all_zero(&db).await;
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn preview_rejects_wrong_guild_without_http_or_writes() {
    let db = Database::open().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    let mut request = interaction(111, "purge-preview", preview_options(10), PERMISSIONS);
    request.guild_id = Some(twilight_model::id::Id::new(999));
    let reply = handled(
        runtime.execute(&router(true), &request).await.unwrap(),
        "purge-preview",
    );
    assert_eq!(reply.outcome, "refused");
    assert_no_requests(&mock);
    assert_all_zero(&db).await;
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn preview_respond_is_ephemeral_without_mentions_or_hostile_echo() {
    let db = Database::open().await;
    // Hostile content must never echo: counts and bounded metadata only.
    let hostile = "@everyone raid now @here <@&999> \u{202e}evil https://x.test/ping";
    let first = fresh_message_id(31);
    let second = fresh_message_id(32);
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(
                200,
                json!([
                    msg(first.clone(), Some(0), false, "10", Some(false), hostile),
                    msg(second.clone(), Some(0), false, "11", Some(false), hostile),
                ]),
            ),
            ScriptedResponse::json(200, json!({"id": CHANNEL})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let request = interaction(112, "purge-preview", preview_options(2), PERMISSIONS);
    assert_eq!(
        runtime.respond(&router(true), &request).await,
        Ok(true),
        "RED: purge-preview has no ephemeral lifecycle yet"
    );
    let requests = mock.requests();
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with("/callback"));
    let defer: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(defer["type"], 5);
    assert_eq!(defer["data"]["flags"], 64);
    let edits: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
    assert_eq!(
        edits.len(),
        1,
        "exactly one @original edit: {:?}",
        requests
            .iter()
            .map(|r| (r.method.clone(), r.path.clone()))
            .collect::<Vec<_>>()
    );
    assert!(
        edits[0].path.contains("/webhooks/") && edits[0].path.ends_with("/messages/@original"),
        "edit targets the interaction webhook: {}",
        edits[0].path
    );
    let edit: Value = serde_json::from_slice(&edits[0].body).unwrap();
    let content = edit["content"].as_str().unwrap();
    assert!(content.contains("targetable: 2"), "{content}");
    assert!(content.contains("not a guarantee"), "{content}");
    for hostile_part in ["@everyone", "@here", "<@&999>", "https://x.test/ping"] {
        assert!(
            !content.contains(hostile_part),
            "hostile message content is never echoed: {content}"
        );
    }
    assert_eq!(edit["allowed_mentions"]["parse"], json!([]));
    assert_no_channel_writes(&mock);
    assert_all_zero(&db).await;
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// channel-inspect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inspect_reports_lockdown_and_slowmode_when_set() {
    let db = Database::open().await;
    seed_lockdown(&db, "raid drill", "2026-10-01T00:00:00.000Z").await;
    let mock = MockRest::start(doc_script(Some(30), true), ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    let before = db.snapshot().await;
    let request = interaction(200, "channel-inspect", inspect_options(), PERMISSIONS);
    let reply = handled(
        runtime.execute(&router(true), &request).await.unwrap(),
        "channel-inspect",
    );
    assert_eq!(reply.outcome, "channel_inspect");
    assert!(
        reply.text.contains("lockdown: active"),
        "store record renders active: {}",
        reply.text
    );
    assert!(
        reply.text.contains("reason: raid drill"),
        "stored reason echoes verbatim: {}",
        reply.text
    );
    assert!(
        reply.text.contains("set at: 2026-10-01"),
        "calendar date comes from the stored locked_at: {}",
        reply.text
    );
    assert!(
        reply.text.contains("set by: unknown"),
        "a directly seeded row has no audit author: {}",
        reply.text
    );
    assert!(
        reply.text.contains("slowmode: 30s"),
        "live delay renders with units: {}",
        reply.text
    );
    assert_channel_read(&mock);
    assert_get_only(&mock);
    assert_eq!(db.snapshot().await, before, "reads change nothing");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn inspect_after_real_lockdown_reports_setter_from_audit() {
    let db = Database::open().await;
    let mut script = vec![overwrite("3072", "8192"), ScriptedResponse::status(204)];
    script.extend(doc_script(Some(0), true));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    let router = router(true);
    let setup = interaction(201, "lockdown", reason_options(None), PERMISSIONS);
    assert_eq!(
        runtime
            .execute(&router, &setup)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "locked_down"
    );
    let before = db.snapshot().await;
    let seen_requests = mock.requests().len();
    let request = interaction(202, "channel-inspect", inspect_options(), PERMISSIONS);
    let reply = handled(
        runtime.execute(&router, &request).await.unwrap(),
        "channel-inspect",
    );
    assert_eq!(reply.outcome, "channel_inspect");
    assert!(reply.text.contains("lockdown: active"), "{}", reply.text);
    assert!(
        reply.text.contains(&format!("set by: {ACTOR}")),
        "the lockdown actor is durably known via audit: {}",
        reply.text
    );
    assert!(
        reply.text.contains("set at: 2026"),
        "a live lockdown has a stored date: {}",
        reply.text
    );
    assert!(
        reply.text.contains("slowmode: off"),
        "a zero delay renders off, not unknown: {}",
        reply.text
    );
    assert!(
        mock.requests()[seen_requests..]
            .iter()
            .all(|r| r.method == "GET"),
        "the inspect itself makes no mutations"
    );
    assert_new_channel_read(&mock, seen_requests);
    assert_eq!(db.snapshot().await, before, "reads change nothing");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn inspect_cleared_lockdown_and_missing_slowmode_render_none() {
    let db = Database::open().await;
    let mut script = vec![
        overwrite("3072", "8192"),
        ScriptedResponse::status(204),
        overwrite("1024", "10240"),
        ScriptedResponse::status(204),
    ];
    script.extend(doc_script(None, false));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    let router = router(true);
    for (id, action) in [(203, "lockdown"), (204, "unlock")] {
        let setup = interaction(id, action, reason_options(None), PERMISSIONS);
        let outcome = runtime
            .execute(&router, &setup)
            .await
            .unwrap()
            .unwrap()
            .outcome;
        assert!(
            matches!(outcome.as_str(), "locked_down" | "unlocked"),
            "{outcome}"
        );
    }
    assert!(db.store.get_lockdown(CHANNEL).await.unwrap().is_none());
    let before = db.snapshot().await;
    let seen_requests = mock.requests().len();
    let request = interaction(205, "channel-inspect", inspect_options(), PERMISSIONS);
    let reply = handled(
        runtime.execute(&router, &request).await.unwrap(),
        "channel-inspect",
    );
    assert_eq!(reply.outcome, "channel_inspect");
    assert!(
        reply.text.contains("lockdown: none"),
        "a cleared record is none, never an error: {}",
        reply.text
    );
    assert!(
        reply.text.contains("slowmode: off"),
        "a missing delay field is off, never an error: {}",
        reply.text
    );
    assert!(
        mock.requests()[seen_requests..]
            .iter()
            .all(|r| r.method == "GET"),
        "the inspect itself makes no mutations (setup lockdown/unlock PUT above)"
    );
    assert_new_channel_read(&mock, seen_requests);
    assert_eq!(db.snapshot().await, before, "reads change nothing");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn inspect_channel_read_failure_reports_slowmode_unknown_with_retry() {
    let db = Database::open().await;
    seed_lockdown(&db, "raid drill", "2026-10-01T00:00:00.000Z").await;
    let mock = MockRest::start(
        vec![ScriptedResponse::status(500)],
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(&db, &mock);
    let before = db.snapshot().await;
    let request = interaction(206, "channel-inspect", inspect_options(), PERMISSIONS);
    let reply = handled(
        runtime.execute(&router(true), &request).await.unwrap(),
        "channel-inspect",
    );
    // Partial success: the store half is intact, the live half is unknown.
    assert_eq!(reply.outcome, "channel_inspect");
    assert!(reply.text.contains("lockdown: active"), "{}", reply.text);
    assert!(
        reply.text.contains("slowmode: unknown"),
        "an unreadable live delay is unknown, never fabricated: {}",
        reply.text
    );
    assert!(
        contains_ci(&reply.text, "try again"),
        "retry guidance accompanies the unknown: {}",
        reply.text
    );
    assert_get_only(&mock);
    assert_eq!(db.snapshot().await, before, "reads change nothing");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn inspect_store_outage_refuses_with_retry_and_zero_effects() {
    let db = Database::open().await;
    seed_lockdown(&db, "raid drill", "2026-10-01T00:00:00.000Z").await;
    let mock = MockRest::start(doc_script(Some(5), true), ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    // True outage: every store read fails from here on. Table counts are
    // unreadable too, so this test pins the reply shape and the HTTP
    // allowlist; the refusal itself proves no write could have landed.
    db.store.pool().close().await;
    let request = interaction(207, "channel-inspect", inspect_options(), PERMISSIONS);
    let reply = handled(
        runtime.execute(&router(true), &request).await.unwrap(),
        "channel-inspect",
    );
    assert_eq!(reply.outcome, "refused");
    assert!(
        contains_ci(&reply.text, "try again"),
        "store outage carries retry guidance: {}",
        reply.text
    );
    assert_get_only(&mock);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn inspect_refuses_non_staff_without_http_or_writes() {
    let db = Database::open().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    // Nobody (0) and ManageMessages-only staff: inspect needs ManageChannels.
    for (id, permissions) in [(208, 0), (209, MANAGE_MESSAGES_ONLY)] {
        let request = interaction(id, "channel-inspect", inspect_options(), permissions);
        let reply = handled(
            runtime.execute(&router(true), &request).await.unwrap(),
            "channel-inspect",
        );
        assert_eq!(reply.outcome, "refused");
    }
    assert_no_requests(&mock);
    assert_all_zero(&db).await;
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn inspect_rejects_wrong_guild_without_http_or_writes() {
    let db = Database::open().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    let mut request = interaction(210, "channel-inspect", inspect_options(), PERMISSIONS);
    request.guild_id = Some(twilight_model::id::Id::new(999));
    let reply = handled(
        runtime.execute(&router(true), &request).await.unwrap(),
        "channel-inspect",
    );
    assert_eq!(reply.outcome, "refused");
    assert_no_requests(&mock);
    assert_all_zero(&db).await;
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn disabled_reads_refuse_without_http_or_writes() {
    let db = Database::open().await;
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    let router = router(false);
    for (id, action, options) in [
        (211, "purge-preview", preview_options(10)),
        (212, "channel-inspect", inspect_options()),
    ] {
        let request = interaction(id, action, options, PERMISSIONS);
        let reply = handled(runtime.execute(&router, &request).await.unwrap(), action);
        assert_eq!(reply.outcome, "refused");
    }
    assert_no_requests(&mock);
    assert_all_zero(&db).await;
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn inspect_respond_is_ephemeral_with_inert_reason_echo() {
    let db = Database::open().await;
    // Staff-authored reasons echo verbatim; allowed_mentions keeps them inert.
    let reason = "@everyone lockdown <@&999> https://x.test/ping";
    seed_lockdown(&db, reason, "2026-10-01T00:00:00.000Z").await;
    let mut script = vec![ScriptedResponse::status(204)];
    script.extend(doc_script(Some(5), true));
    script.push(ScriptedResponse::json(200, json!({"id": CHANNEL})));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let runtime = runtime(&db, &mock);
    let before = db.snapshot().await;
    let request = interaction(213, "channel-inspect", inspect_options(), PERMISSIONS);
    assert_eq!(
        runtime.respond(&router(true), &request).await,
        Ok(true),
        "RED: channel-inspect has no ephemeral lifecycle yet"
    );
    let requests = mock.requests();
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with("/callback"));
    let defer: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(defer["type"], 5);
    assert_eq!(defer["data"]["flags"], 64);
    let edits: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
    assert_eq!(
        edits.len(),
        1,
        "exactly one @original edit: {:?}",
        requests
            .iter()
            .map(|r| (r.method.clone(), r.path.clone()))
            .collect::<Vec<_>>()
    );
    assert!(
        edits[0].path.contains("/webhooks/") && edits[0].path.ends_with("/messages/@original"),
        "edit targets the interaction webhook: {}",
        edits[0].path
    );
    let edit: Value = serde_json::from_slice(&edits[0].body).unwrap();
    let content = edit["content"].as_str().unwrap();
    assert!(content.contains("lockdown: active"), "{content}");
    assert!(
        content.contains(&format!("reason: {reason}")),
        "stored reason echoes verbatim: {content}"
    );
    assert!(content.contains("slowmode: 5s"), "{content}");
    assert_eq!(edit["allowed_mentions"]["parse"], json!([]));
    assert_no_channel_writes(&mock);
    assert_eq!(db.snapshot().await, before, "reads change nothing");
    mock.shutdown().await;
    db.close().await;
}
