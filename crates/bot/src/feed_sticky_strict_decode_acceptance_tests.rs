//! Protected acceptance corpus: strict `/feed-add`, `/feed-remove` and
//! `/sticky` option decoding.
//!
//! Contract under test: a malformed option (wrong wire type, a duplicated
//! option, or a missing required option) is refused BEFORE any store read or
//! mutation. The refusal is one private (ephemeral defer + single edit)
//! response that names the offending option and shows a copyable corrected
//! invocation in the established `/feed-list` style
//! ("Invalid page. Use /feed-list page:<number> ..."). Refusal leaves every
//! persistent relation byte-identical: no relay row, no delivery-ledger
//! touch, no sticky row, no schedule row, no audit row, no publish.
//!
//! Decode-malformed is distinct from value-invalid, and the tests pin the
//! line: an omitted `source` is a decode refusal (names `source`, no audit),
//! while a present-but-empty `source` keeps today's SSRF-guard refusal; an
//! omitted `body` is a decode refusal (no audit), while a present-but-empty
//! `body` keeps today's rejected-and-audited validation shape. Unknown kind
//! strings, out-of-range debounce values and the strict `page` decoder keep
//! their exact current behavior as controls.
//!
//! These tests never call the option decoders directly: the decoder
//! signatures are the implementation's to change, and a protected test that
//! named them would not compile after the fix. Every case dispatches a real
//! interaction through the shared runtime and asserts observable behavior
//! (reply text plus persistent state), so RED means wrong behavior, never a
//! missing symbol.
#![cfg(test)]

use super::{
    db_runtime, option, slash, CommandOptionValue, MockRest, TestDb, CHANNEL, CHANNEL_S, GUILD_S,
};

/// Refusals stay short enough to read at a glance (today's longest refusal,
/// the SSRF guard, is ~100 chars; Discord allows 2,000).
const REFUSAL_MAX_CHARS: usize = 500;
/// Other-guild relay proving refusals and valid writes stay guild-scoped.
const FOREIGN_GUILD: &str = "9999";
const FOREIGN_RELAY: &str = "feed-foreign";
const KEEP_RELAY: &str = "feed-keep";
/// Today's locked SSRF-guard text (controls pin it byte-identical).
const SSRF_REFUSAL: &str =
    "Feed source must be an HTTPS URL on port 443, without credentials, and at most 2048 bytes.";

/// Whole-relation snapshot: one stable rendering per table. Equality means
/// byte-identical persistent state (rows, columns and timestamps).
#[derive(Debug, PartialEq, Eq)]
struct PersistentState {
    relays: String,
    deliveries: String,
    stickies: String,
    scheduled: String,
    announcements: String,
    automation: String,
}

async fn table_text(db: &TestDb, query: &'static str) -> String {
    sqlx::query_scalar::<_, String>(query)
        .fetch_one(&db.pool)
        .await
        .expect("state snapshot")
}

async fn persistent_state(db: &TestDb) -> PersistentState {
    PersistentState {
        relays: table_text(
            db,
            "SELECT COALESCE(string_agg(t::text, E'\n' ORDER BY id), '') FROM feed_relays t",
        )
        .await,
        deliveries: table_text(
            db,
            "SELECT COALESCE(string_agg(t::text, E'\n' ORDER BY feed_id, item_key), '') \
             FROM feed_deliveries t",
        )
        .await,
        stickies: table_text(
            db,
            "SELECT COALESCE(string_agg(t::text, E'\n' ORDER BY guild_id, channel_id), '') \
             FROM sticky_messages t",
        )
        .await,
        scheduled: table_text(
            db,
            "SELECT COALESCE(string_agg(t::text, E'\n' ORDER BY id), '') \
             FROM scheduled_messages t",
        )
        .await,
        announcements: table_text(
            db,
            "SELECT COALESCE(string_agg(t::text, E'\n' ORDER BY guild_id, created_at, id), '') \
             FROM announcements_audit_log t",
        )
        .await,
        automation: table_text(
            db,
            "SELECT COALESCE(string_agg(t::text, E'\n' ORDER BY guild_id, created_at, id), '') \
             FROM automation_audit_log t",
        )
        .await,
    }
}

/// Representative pre-state: a live relay with pending delivery evidence, an
/// other-guild relay, and a posted sticky. Nothing here may move on a
/// refusal path.
async fn seed_refusal_fixture(db: &TestDb) {
    db.seed_feed(GUILD_S, KEEP_RELAY, "rss").await;
    db.seed_feed(FOREIGN_GUILD, FOREIGN_RELAY, "rss").await;
    sqlx::query(
        "INSERT INTO feed_deliveries
           (feed_id, item_key, nonce, state, message_id, first_seen_at, delivered_at)
         VALUES ($1, 'item-0', 'nonce-strict-refusal-0', 'pending', NULL,
                 clock_timestamp(), NULL)",
    )
    .bind(KEEP_RELAY)
    .execute(&db.pool)
    .await
    .expect("seed pending delivery");
    db.seed(Some("8000000000000000001"), 120_000, 5).await;
}

/// The strict-refusal shape: names `option`, shows a copyable `command`
/// invocation, stays bounded and mention-safe. Returns false instead of
/// panicking so the shape demo can assert both legs.
fn is_strict_refusal(content: &str, option: &str, command: &str) -> bool {
    // The option name matches case-insensitively ("Debounce" and "debounce"
    // both name the option); the corrected invocation names the command.
    let lowered = content.to_lowercase();
    !content.is_empty()
        && lowered.contains(option)
        && content.contains(command)
        && content.chars().count() <= REFUSAL_MAX_CHARS
        && !content.contains("@everyone")
        && !content.contains("@here")
        && !content.contains("<@")
}

fn assert_strict_refusal(content: &str, option: &str, command: &str) {
    assert!(
        is_strict_refusal(content, option, command),
        "refusal names `{option}` with a copyable `{command}` invocation, bounded and \
         mention-safe; got: {content:?}"
    );
}

/// Exactly one private response: the ephemeral defer plus a single
/// completion edit (asserted inside `deferred_reply`, with an empty
/// mention-parse set), and no other REST traffic (no publish, no repost).
fn single_private_reply(mock: &MockRest) -> String {
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        2,
        "defer plus one completion, nothing else: {:?}",
        requests
            .iter()
            .map(|r| (&r.method, &r.path))
            .collect::<Vec<_>>()
    );
    assert!(
        requests.iter().all(|r| r.method != "PUT"),
        "no registry publish on a command path"
    );
    mock.deferred_reply()["content"]
        .as_str()
        .expect("completion content")
        .to_owned()
}

fn feed_add(options: Vec<super::CommandDataOption>) -> super::Interaction {
    slash("feed-add", Some(CHANNEL), options)
}

fn feed_remove(options: Vec<super::CommandDataOption>) -> super::Interaction {
    slash("feed-remove", Some(CHANNEL), options)
}

fn sticky(options: Vec<super::CommandDataOption>) -> super::Interaction {
    slash("sticky", Some(CHANNEL), options)
}

// ---------------------------------------------------------------------------
// RED: `/feed-add` strict decode refusals
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_add_wrong_typed_kind_refuses_before_any_read_or_write() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_add(vec![
            option("kind", CommandOptionValue::Integer(1)),
            option(
                "source",
                CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
            ),
        ]))
        .await;

    let reply = single_private_reply(&mock);
    assert_strict_refusal(&reply, "kind", "/feed-add");
    assert_eq!(
        persistent_state(&db).await,
        before,
        "wrong-typed kind writes nothing"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_add_wrong_typed_source_refuses_before_any_read_or_write() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_add(vec![
            option("kind", CommandOptionValue::String("rss".to_owned())),
            option("source", CommandOptionValue::Integer(7)),
        ]))
        .await;

    let reply = single_private_reply(&mock);
    assert_strict_refusal(&reply, "source", "/feed-add");
    assert_eq!(
        persistent_state(&db).await,
        before,
        "wrong-typed source writes nothing"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_add_duplicated_kind_refuses_without_mutation() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_add(vec![
            option("kind", CommandOptionValue::String("rss".to_owned())),
            option("kind", CommandOptionValue::String("rss".to_owned())),
            option(
                "source",
                CommandOptionValue::String("https://example.com/dup-kind.xml".to_owned()),
            ),
        ]))
        .await;

    let reply = single_private_reply(&mock);
    assert_strict_refusal(&reply, "kind", "/feed-add");
    assert_eq!(
        persistent_state(&db).await,
        before,
        "duplicated kind creates no relay and no audit"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_add_duplicated_source_refuses_without_mutation() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_add(vec![
            option("kind", CommandOptionValue::String("rss".to_owned())),
            option(
                "source",
                CommandOptionValue::String("https://example.com/dup-source.xml".to_owned()),
            ),
            option(
                "source",
                CommandOptionValue::String("https://example.com/dup-source.xml".to_owned()),
            ),
        ]))
        .await;

    let reply = single_private_reply(&mock);
    assert_strict_refusal(&reply, "source", "/feed-add");
    assert_eq!(
        persistent_state(&db).await,
        before,
        "duplicated source creates no relay and no audit"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_add_missing_kind_names_the_option() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_add(vec![option(
            "source",
            CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
        )]))
        .await;

    let reply = single_private_reply(&mock);
    assert_strict_refusal(&reply, "kind", "/feed-add");
    assert_eq!(
        persistent_state(&db).await,
        before,
        "missing kind writes nothing"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_add_missing_source_names_the_option() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_add(vec![option(
            "kind",
            CommandOptionValue::String("rss".to_owned()),
        )]))
        .await;

    let reply = single_private_reply(&mock);
    assert_strict_refusal(&reply, "source", "/feed-add");
    assert_eq!(
        persistent_state(&db).await,
        before,
        "missing source writes nothing"
    );
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// RED: `/feed-remove` strict decode refusals
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_remove_wrong_typed_id_refuses_before_any_read_or_write() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_remove(vec![option(
            "id",
            CommandOptionValue::Integer(42),
        )]))
        .await;

    let reply = single_private_reply(&mock);
    assert_strict_refusal(&reply, "id", "/feed-remove");
    assert_eq!(
        persistent_state(&db).await,
        before,
        "wrong-typed id writes nothing"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_remove_duplicated_id_retains_relay_and_writes_no_audit() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_remove(vec![
            option("id", CommandOptionValue::String(KEEP_RELAY.to_owned())),
            option("id", CommandOptionValue::String(KEEP_RELAY.to_owned())),
        ]))
        .await;

    let reply = single_private_reply(&mock);
    assert_strict_refusal(&reply, "id", "/feed-remove");
    assert_eq!(
        persistent_state(&db).await,
        before,
        "duplicated id deletes nothing (relay and its delivery ledger survive) and audits nothing"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_remove_missing_id_names_the_option() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime.on_interaction(&feed_remove(Vec::new())).await;

    let reply = single_private_reply(&mock);
    assert_strict_refusal(&reply, "id", "/feed-remove");
    assert_eq!(
        persistent_state(&db).await,
        before,
        "missing id writes nothing"
    );
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// RED: `/sticky` strict decode refusals
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_wrong_typed_body_refuses_without_audit() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&sticky(vec![option(
            "body",
            CommandOptionValue::Integer(9),
        )]))
        .await;

    let reply = single_private_reply(&mock);
    assert_strict_refusal(&reply, "body", "/sticky");
    assert_eq!(
        persistent_state(&db).await,
        before,
        "wrong-typed body neither upserts nor audits"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_wrong_typed_debounce_refuses_without_mutation() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&sticky(vec![
            option("body", CommandOptionValue::String("pin me".to_owned())),
            option("debounce", CommandOptionValue::String("soon".to_owned())),
        ]))
        .await;

    let reply = single_private_reply(&mock);
    assert_strict_refusal(&reply, "debounce", "/sticky");
    assert_eq!(
        persistent_state(&db).await,
        before,
        "wrong-typed debounce never becomes the default: no upsert, no audit"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_duplicated_body_refuses_without_mutation() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&sticky(vec![
            option("body", CommandOptionValue::String("pin me".to_owned())),
            option("body", CommandOptionValue::String("pin me".to_owned())),
        ]))
        .await;

    let reply = single_private_reply(&mock);
    assert_strict_refusal(&reply, "body", "/sticky");
    assert_eq!(
        persistent_state(&db).await,
        before,
        "duplicated body neither upserts nor audits"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_duplicated_debounce_refuses_without_mutation() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&sticky(vec![
            option("body", CommandOptionValue::String("pin me".to_owned())),
            option("debounce", CommandOptionValue::Integer(7)),
            option("debounce", CommandOptionValue::Integer(7)),
        ]))
        .await;

    let reply = single_private_reply(&mock);
    assert_strict_refusal(&reply, "debounce", "/sticky");
    assert_eq!(
        persistent_state(&db).await,
        before,
        "duplicated debounce neither upserts nor audits"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_missing_body_names_the_option_without_audit() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&sticky(vec![option(
            "debounce",
            CommandOptionValue::Integer(7),
        )]))
        .await;

    let reply = single_private_reply(&mock);
    assert_strict_refusal(&reply, "body", "/sticky");
    assert_eq!(
        persistent_state(&db).await,
        before,
        "missing body neither upserts nor audits"
    );
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// Controls: valid invocations and value-invalid refusals stay byte-identical
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_add_valid_relay_is_byte_identical_to_today() {
    let db = TestDb::new().await;
    db.seed_feed(FOREIGN_GUILD, FOREIGN_RELAY, "rss").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_add(vec![
            option("kind", CommandOptionValue::String("rss".to_owned())),
            option(
                "source",
                CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
            ),
        ]))
        .await;

    let rows = db.feed_rows().await;
    assert_eq!(rows.len(), 1, "one relay row written");
    let (id, guild_id, channel_id, kind, source) = &rows[0];
    assert_eq!(guild_id, GUILD_S);
    assert_eq!(channel_id, CHANNEL_S, "relay binds the invoking channel");
    assert_eq!(kind, "rss");
    assert_eq!(source, "https://example.com/feed.xml");
    let reply = single_private_reply(&mock);
    assert_eq!(reply, format!("Feed relay created: `{id}`."));
    assert_eq!(
        db.feed_audits().await,
        vec![("feed.create".to_owned(), id.clone(), "rss".to_owned())]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_add_ssrf_guard_refusal_is_unchanged() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_add(vec![
            option("kind", CommandOptionValue::String("rss".to_owned())),
            option(
                "source",
                CommandOptionValue::String("http://169.254.169.254/latest".to_owned()),
            ),
        ]))
        .await;

    assert_eq!(single_private_reply(&mock), SSRF_REFUSAL);
    assert_eq!(persistent_state(&db).await, before);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_add_unknown_kind_string_refusal_is_unchanged() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_add(vec![
            option("kind", CommandOptionValue::String("atom".to_owned())),
            option(
                "source",
                CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
            ),
        ]))
        .await;

    // A well-typed but unknown kind is value-invalid, not decode-malformed:
    // today's domain refusal stands.
    assert_eq!(single_private_reply(&mock), "Unknown feed kind.");
    assert_eq!(persistent_state(&db).await, before);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_add_empty_source_string_refusal_is_unchanged() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_add(vec![
            option("kind", CommandOptionValue::String("rss".to_owned())),
            option("source", CommandOptionValue::String(String::new())),
        ]))
        .await;

    // Present-but-empty is value-invalid (SSRF guard), unlike an omitted
    // `source`, which is a decode refusal naming the option.
    assert_eq!(single_private_reply(&mock), SSRF_REFUSAL);
    assert_eq!(persistent_state(&db).await, before);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_remove_present_and_missing_replies_are_unchanged() {
    let db = TestDb::new().await;
    db.seed_feed(GUILD_S, "feed-drop", "rss").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_remove(vec![option(
            "id",
            CommandOptionValue::String("feed-drop".to_owned()),
        )]))
        .await;
    runtime
        .on_interaction(&feed_remove(vec![option(
            "id",
            CommandOptionValue::String("feed-absent".to_owned()),
        )]))
        .await;

    let rows = db.feed_rows().await;
    assert!(rows.is_empty(), "only the targeted relay is deleted");
    let edits: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    assert_eq!(edits.len(), 2, "one completion per remove");
    let first: serde_json::Value = serde_json::from_slice(&edits[0].body).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&edits[1].body).unwrap();
    assert_eq!(first["content"], "Feed relay removed.");
    assert_eq!(second["content"], "No feed relay with that id.");
    assert_eq!(
        db.feed_audits().await,
        vec![
            (
                "feed.remove".to_owned(),
                "feed-drop".to_owned(),
                "removed".to_owned()
            ),
            (
                "feed.remove".to_owned(),
                "feed-absent".to_owned(),
                "missing".to_owned()
            ),
        ]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_remove_empty_string_id_refusal_is_unchanged() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_remove(vec![option(
            "id",
            CommandOptionValue::String(String::new()),
        )]))
        .await;

    // Present-but-empty is value-invalid (`validate_id`), unlike an omitted
    // `id`, which is a decode refusal naming the option.
    assert_eq!(single_private_reply(&mock), "Invalid feed id.");
    assert_eq!(persistent_state(&db).await, before);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_valid_body_and_debounce_is_byte_identical_to_today() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&sticky(vec![
            option("body", CommandOptionValue::String("pin me".to_owned())),
            option("debounce", CommandOptionValue::Integer(7)),
        ]))
        .await;

    let (body, _, _) = db.row().await.expect("row written");
    assert_eq!(body, "pin me");
    let debounce: i32 = sqlx::query_scalar(
        "SELECT debounce_seconds FROM sticky_messages WHERE guild_id = $1 AND channel_id = $2",
    )
    .bind(GUILD_S)
    .bind(CHANNEL_S)
    .fetch_one(&db.pool)
    .await
    .expect("debounce");
    assert_eq!(debounce, 7);
    assert_eq!(
        single_private_reply(&mock),
        "Sticky set for <#3333>, 7s debounce."
    );
    assert_eq!(
        db.audits().await,
        vec![("sticky.create".to_owned(), "ok".to_owned())]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_omitted_debounce_uses_default_and_succeeds() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&sticky(vec![option(
            "body",
            CommandOptionValue::String("pin me".to_owned()),
        )]))
        .await;

    // `debounce` is the missing-optional case: omission succeeds with the
    // default quiet window and no debounce suffix in the confirmation.
    let debounce: i32 = sqlx::query_scalar(
        "SELECT debounce_seconds FROM sticky_messages WHERE guild_id = $1 AND channel_id = $2",
    )
    .bind(GUILD_S)
    .bind(CHANNEL_S)
    .fetch_one(&db.pool)
    .await
    .expect("debounce");
    assert_eq!(debounce, 5);
    assert_eq!(single_private_reply(&mock), "Sticky set for <#3333>.");
    assert_eq!(
        db.audits().await,
        vec![("sticky.create".to_owned(), "ok".to_owned())]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_empty_body_rejection_still_audits() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&sticky(vec![option(
            "body",
            CommandOptionValue::String(String::new()),
        )]))
        .await;

    // Present-but-empty is value-invalid: today's rejected-and-audited
    // validation shape stands, unlike an omitted `body`.
    assert!(db.row().await.is_none(), "no row on rejection");
    assert_eq!(
        single_private_reply(&mock),
        "Sticky body must be between 1 and 2000 characters."
    );
    assert_eq!(
        db.audits().await,
        vec![("sticky.create".to_owned(), "rejected".to_owned())]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_out_of_range_debounce_rejection_still_audits() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&sticky(vec![
            option("body", CommandOptionValue::String("pin me".to_owned())),
            option("debounce", CommandOptionValue::Integer(999)),
        ]))
        .await;

    // Well-typed but out-of-range is value-invalid: today's rejected and
    // audited shape stands, unlike a wrong-typed `debounce`.
    assert!(db.row().await.is_none(), "no row on rejection");
    assert_eq!(
        single_private_reply(&mock),
        "Debounce must be between 1 and 300 seconds."
    );
    assert_eq!(
        db.audits().await,
        vec![("sticky.create".to_owned(), "rejected".to_owned())]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_list_wrong_typed_page_refusal_is_unchanged() {
    let db = TestDb::new().await;
    seed_refusal_fixture(&db).await;
    let before = persistent_state(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash(
            "feed-list",
            Some(CHANNEL),
            vec![option(
                "page",
                CommandOptionValue::String("second".to_owned()),
            )],
        ))
        .await;

    // The established strict-`page` refusal this work extends: exact text,
    // no audit, one private response.
    assert_eq!(
        single_private_reply(&mock),
        "Invalid page. Use /feed-list page:<number> with a whole number from 1."
    );
    assert_eq!(persistent_state(&db).await, before);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_list_omitted_page_lists_first_page() {
    let db = TestDb::new().await;
    db.seed_feed(GUILD_S, KEEP_RELAY, "rss").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash("feed-list", Some(CHANNEL), Vec::new()))
        .await;

    assert_eq!(
        single_private_reply(&mock),
        "`feed-keep` rss → <#3333> https://example.com/feed-keep.xml · Last attempt: never\nRun /feed-inspect id:<relay-id> for source and last-attempt detail."
    );
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// Shape demo (always runs): the refusal predicate accepts the intended
// correction shape and rejects today's generic texts, so the RED legs above
// fail on behavior and the GREEN contract is satisfiable.
// ---------------------------------------------------------------------------

#[test]
fn refusal_predicates_accept_the_intended_shape_and_reject_todays_generics() {
    for (content, option, command) in [
        (
            "Invalid kind. Use /feed-add kind:rss source:<https-url>.",
            "kind",
            "/feed-add",
        ),
        (
            "Invalid source. Use /feed-add kind:rss source:<https-url>.",
            "source",
            "/feed-add",
        ),
        (
            "Invalid id. Use /feed-remove id:<relay-id>.",
            "id",
            "/feed-remove",
        ),
        (
            "Invalid body. Use /sticky body:<text> debounce:<seconds>.",
            "body",
            "/sticky",
        ),
        (
            "Invalid debounce. Use /sticky body:<text> debounce:<seconds>.",
            "debounce",
            "/sticky",
        ),
    ] {
        assert!(
            is_strict_refusal(content, option, command),
            "intended shape passes: {content:?}"
        );
    }
    // Today's generic texts carry no corrected invocation, so every RED leg
    // above fails on current behavior for the right reason.
    for content in [
        "Unknown feed kind.",
        "Invalid feed id.",
        "Sticky body must be between 1 and 2000 characters.",
        "Debounce must be between 1 and 300 seconds.",
        SSRF_REFUSAL,
        "Sticky set for <#3333>, 7s debounce.",
    ] {
        for (option, command) in [
            ("kind", "/feed-add"),
            ("source", "/feed-add"),
            ("id", "/feed-remove"),
            ("body", "/sticky"),
            ("debounce", "/sticky"),
        ] {
            assert!(
                !is_strict_refusal(content, option, command),
                "today's text carries no correction: {content:?}"
            );
        }
    }
}
