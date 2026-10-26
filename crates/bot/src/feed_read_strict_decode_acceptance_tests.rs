//! Protected acceptance corpus: strict `/feed-inspect` and `/feed-list`
//! option decoding.
//!
//! Contract under test: both read shapes decode strictly before any saved
//! relay is read. `/feed-inspect` requires exactly one string `id` and no
//! other options; `/feed-list` allows at most one integer `page` (omitted
//! means page 1) and no other options. A duplicated, wrong-typed, unknown
//! or nested option — in either order — is refused with one bounded,
//! ephemeral, mention-safe correction naming the expected option and the
//! corrected invocation, exactly like the established strict writer
//! refusals ("Duplicate id. Use /feed-remove id:<relay-id>."). The refusal
//! echoes no supplied value, relay, source or raw error, and performs no
//! store access, audit write, fetch or Discord effect beyond the reply.
//!
//! Decode-malformed stays distinct from value-invalid, mirroring the
//! writer corpus: a present single string id that fails id validation
//! keeps "Invalid feed id.", an integer page below 1 keeps the underflow
//! text, and valid shapes keep today's missing/foreign-relay and
//! storage-unavailable distinctions, page behaviour and unaudited reads.
//!
//! These tests never call the option decoders directly: every case
//! dispatches a real interaction through the shared runtime and asserts
//! observable behaviour, so RED means wrong behaviour, never a missing
//! symbol. The always-run layer uses a closed pool as the store-access
//! oracle — any store read on it answers the fixed store-failure text,
//! which the healthy controls prove valid shapes still reach — so a
//! strict correction on that pool can only mean refusal before the read.
//! The `#[ignore]` layer replays the matrix against the disposable test
//! database and pins byte-identical saved relays and an empty audit log.
#![cfg(test)]

use std::sync::Arc;

use super::{
    db_runtime, executor_at, gates, option, patch_contents, router_with_commands, slash,
    CommandDataOption, CommandOptionValue, CommandRuntime, Interaction, MockRest, PgPoolOptions,
    TestDb, CHANNEL, FEED_STORE_FAILURE_REPLY, GUILD, GUILD_S,
};

/// Refusals stay short enough to read at a glance (today's longest feed
/// refusal is ~100 chars; Discord allows 2,000).
const REFUSAL_MAX_CHARS: usize = 500;
/// Marker planted in supplied values: a correction must never echo them.
const LEAK_MARKER: &str = "leakcheck";
const FOREIGN_GUILD: &str = "9999";

fn string_id(value: &str) -> CommandDataOption {
    option("id", CommandOptionValue::String(value.to_owned()))
}

fn inspect(options: Vec<CommandDataOption>) -> Interaction {
    slash("feed-inspect", Some(CHANNEL), options)
}

fn list(options: Vec<CommandDataOption>) -> Interaction {
    slash("feed-list", Some(CHANNEL), options)
}

/// Runtime over a closed pool: every store query fails immediately with
/// the fixed safe reply, so reaching the store is observable in the reply
/// text without a database.
async fn dead_pool_runtime() -> (Arc<CommandRuntime>, MockRest) {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    pool.close().await;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );
    (runtime, mock)
}

/// Exactly one private response: the ephemeral defer plus a single
/// completion edit (asserted inside `deferred_reply`, with an empty
/// mention-parse set), and no other REST traffic.
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
    mock.deferred_reply()["content"]
        .as_str()
        .expect("completion content")
        .to_owned()
}

/// The strict-refusal shape: names the expected `option`, shows the
/// corrected `command` invocation, stays bounded and mention-safe, and
/// leaks no supplied value or store detail. On the dead pool, not being
/// the store-failure text additionally proves no store read happened.
fn assert_strict_refusal(content: &str, option_name: &str, command: &str) {
    let lowered = content.to_lowercase();
    assert!(
        !content.is_empty()
            && lowered.contains(option_name)
            && content.contains(command)
            && content.chars().count() <= REFUSAL_MAX_CHARS
            && !content.contains("@everyone")
            && !content.contains("@here")
            && !content.contains("<@")
            && content != FEED_STORE_FAILURE_REPLY
            && !lowered.contains(LEAK_MARKER)
            && !lowered.contains("example.com")
            && !lowered.contains("sqlx")
            && !lowered.contains("postgres"),
        "refusal names `{option_name}` with a copyable `{command}` invocation, bounded, \
         mention-safe and leak-free, before any store read; got: {content:?}"
    );
}

async fn dispatch_on_dead_pool(interaction: &Interaction) -> String {
    let (runtime, mock) = dead_pool_runtime().await;
    runtime.on_interaction(interaction).await;
    let reply = single_private_reply(&mock);
    mock.shutdown().await;
    reply
}

// ---------------------------------------------------------------------------
// RED: `/feed-inspect` malformed shapes refuse before any store read
// ---------------------------------------------------------------------------

#[tokio::test]
async fn feed_read_strict_inspect_malformed_shapes_refuse_before_store_read() {
    let valid = format!("feed-{LEAK_MARKER}-own");
    let other = format!("feed-{LEAK_MARKER}-other");
    let cases: Vec<(&str, Interaction)> = vec![
        ("missing id", inspect(Vec::new())),
        (
            "duplicate string ids",
            inspect(vec![string_id(&valid), string_id(&other)]),
        ),
        (
            "duplicate string ids reversed",
            inspect(vec![string_id(&other), string_id(&valid)]),
        ),
        (
            "string then wrong-typed id",
            inspect(vec![
                string_id(&valid),
                option("id", CommandOptionValue::Integer(42)),
            ]),
        ),
        (
            "wrong-typed then string id",
            inspect(vec![
                option("id", CommandOptionValue::Integer(42)),
                string_id(&valid),
            ]),
        ),
        (
            "wrong-typed id alone (integer)",
            inspect(vec![option("id", CommandOptionValue::Integer(42))]),
        ),
        (
            "wrong-typed id alone (boolean)",
            inspect(vec![option("id", CommandOptionValue::Boolean(true))]),
        ),
        (
            "wrong-typed id alone (number)",
            inspect(vec![option("id", CommandOptionValue::Number(1.5))]),
        ),
        (
            "unknown option after valid id",
            inspect(vec![
                string_id(&valid),
                option("extra", CommandOptionValue::Boolean(true)),
            ]),
        ),
        (
            "unknown option before valid id",
            inspect(vec![
                option(
                    "extra",
                    CommandOptionValue::String(format!("{LEAK_MARKER}-value")),
                ),
                string_id(&valid),
            ]),
        ),
        (
            "unknown option alone",
            inspect(vec![option("extra", CommandOptionValue::Boolean(true))]),
        ),
        (
            "nested value as id",
            inspect(vec![option(
                "id",
                CommandOptionValue::SubCommand(vec![string_id(&valid)]),
            )]),
        ),
        (
            "nested group alongside valid id",
            inspect(vec![
                string_id(&valid),
                option(
                    "group",
                    CommandOptionValue::SubCommandGroup(vec![string_id(&other)]),
                ),
            ]),
        ),
    ];
    for (label, interaction) in &cases {
        let reply = dispatch_on_dead_pool(interaction).await;
        assert_strict_refusal(&reply, "id", "/feed-inspect");
        assert!(
            !reply.contains("Feed relay `"),
            "{label}: malformed inspect renders no relay detail: {reply:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// RED: `/feed-list` malformed shapes refuse before any store read
// ---------------------------------------------------------------------------

#[tokio::test]
async fn feed_read_strict_list_malformed_shapes_refuse_before_store_read() {
    let page = |value: i64| option("page", CommandOptionValue::Integer(value));
    let cases: Vec<(&str, Interaction)> = vec![
        ("duplicate pages", list(vec![page(1), page(2)])),
        ("duplicate pages reversed", list(vec![page(2), page(1)])),
        (
            "integer then wrong-typed page",
            list(vec![
                page(1),
                option(
                    "page",
                    CommandOptionValue::String(format!("{LEAK_MARKER}-two")),
                ),
            ]),
        ),
        (
            "wrong-typed then integer page",
            list(vec![
                option(
                    "page",
                    CommandOptionValue::String(format!("{LEAK_MARKER}-two")),
                ),
                page(1),
            ]),
        ),
        (
            "wrong-typed page alone (string)",
            list(vec![option(
                "page",
                CommandOptionValue::String("second".to_owned()),
            )]),
        ),
        (
            "wrong-typed page alone (number)",
            list(vec![option("page", CommandOptionValue::Number(1.5))]),
        ),
        (
            "wrong-typed page alone (boolean)",
            list(vec![option("page", CommandOptionValue::Boolean(true))]),
        ),
        (
            "unknown option after valid page",
            list(vec![
                page(1),
                option("extra", CommandOptionValue::Boolean(true)),
            ]),
        ),
        (
            "unknown option before valid page",
            list(vec![
                option("extra", CommandOptionValue::Boolean(true)),
                page(1),
            ]),
        ),
        (
            "unknown option alone",
            list(vec![option("extra", CommandOptionValue::Boolean(true))]),
        ),
        (
            "nested value as page",
            list(vec![option(
                "page",
                CommandOptionValue::SubCommand(vec![page(1)]),
            )]),
        ),
        (
            "nested group alongside absent page",
            list(vec![option(
                "group",
                CommandOptionValue::SubCommandGroup(vec![page(2)]),
            )]),
        ),
    ];
    for (label, interaction) in &cases {
        let reply = dispatch_on_dead_pool(interaction).await;
        assert_strict_refusal(&reply, "page", "/feed-list");
        assert!(
            !reply.contains("Page ") || reply.contains("/feed-list"),
            "{label}: malformed list renders no relay page: {reply:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Healthy controls (pass before and after): valid shapes still reach the
// store, and value-invalid shapes keep their existing pre-store refusals.
// On the dead pool, "reaches the store" is exactly the fixed failure text,
// which also pins the storage-unavailable distinction for valid reads.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn feed_read_strict_valid_shapes_still_reach_the_store() {
    let cases: Vec<(&str, Interaction)> = vec![
        (
            "inspect one string id",
            inspect(vec![string_id("feed-own")]),
        ),
        ("list absent page defaults", list(Vec::new())),
        (
            "list page 1 (lower bound)",
            list(vec![option("page", CommandOptionValue::Integer(1))]),
        ),
        (
            "list page 2",
            list(vec![option("page", CommandOptionValue::Integer(2))]),
        ),
        (
            "list extreme page decodes (range is the composer's)",
            list(vec![option("page", CommandOptionValue::Integer(i64::MAX))]),
        ),
    ];
    for (label, interaction) in &cases {
        let reply = dispatch_on_dead_pool(interaction).await;
        assert_eq!(
            reply, FEED_STORE_FAILURE_REPLY,
            "{label}: valid shape passes decode and reaches the (closed) store"
        );
    }
}

#[tokio::test]
async fn feed_read_strict_value_invalid_shapes_keep_existing_refusals() {
    // A present single string id that fails id validation is value-invalid,
    // not decode-malformed: the planner's distinct copy is retained.
    for bad in ["bad id", ""] {
        let reply = dispatch_on_dead_pool(&inspect(vec![string_id(bad)])).await;
        assert_eq!(reply, "Invalid feed id.", "value-invalid id {bad:?}");
    }
    // An integer page below 1 keeps the existing underflow correction.
    for value in [0, -2] {
        let reply = dispatch_on_dead_pool(&list(vec![option(
            "page",
            CommandOptionValue::Integer(value),
        )]))
        .await;
        assert_eq!(
            reply, "Page must be at least 1. Use /feed-list page:1 to start.",
            "underflow page {value}"
        );
    }
}

// ---------------------------------------------------------------------------
// DB layer: zero mutation / zero audit on refusals, and unchanged valid
// reads (including a disabled/paused relay) against saved relays.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_read_strict_malformed_invocations_leave_saved_relays_and_audit_untouched() {
    let db = TestDb::new().await;
    db.seed_feed(GUILD_S, "feed-own", "rss").await;
    db.seed_feed(FOREIGN_GUILD, "feed-foreign", "twitch").await;
    let rows_before = db.feed_rows().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    let malformed: Vec<Interaction> = vec![
        inspect(vec![string_id("feed-own"), string_id("feed-foreign")]),
        inspect(vec![
            string_id("feed-own"),
            option("extra", CommandOptionValue::Boolean(true)),
        ]),
        list(vec![
            option("page", CommandOptionValue::Integer(1)),
            option("page", CommandOptionValue::Integer(2)),
        ]),
        list(vec![
            option("page", CommandOptionValue::Integer(1)),
            option(
                "page",
                CommandOptionValue::String(format!("{LEAK_MARKER}-two")),
            ),
        ]),
        list(vec![option("extra", CommandOptionValue::Boolean(true))]),
    ];
    let expected: Vec<(&str, &str)> = vec![
        ("id", "/feed-inspect"),
        ("id", "/feed-inspect"),
        ("page", "/feed-list"),
        ("page", "/feed-list"),
        ("page", "/feed-list"),
    ];
    for interaction in &malformed {
        runtime.on_interaction(interaction).await;
    }

    // `patch_contents` pins mention safety on every completion.
    let contents = patch_contents(&mock);
    assert_eq!(contents.len(), malformed.len(), "one completion each");
    for (content, (option_name, command)) in contents.iter().zip(&expected) {
        assert_strict_refusal(content, option_name, command);
        assert!(
            !content.contains("feed-own") && !content.contains("feed-foreign"),
            "refusal discloses no saved relay: {content:?}"
        );
    }
    assert_eq!(
        db.feed_rows().await,
        rows_before,
        "malformed reads leave saved relays unchanged"
    );
    assert!(
        db.feed_audits().await.is_empty(),
        "malformed reads write no audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_read_strict_valid_reads_of_saved_relays_are_unchanged() {
    let db = TestDb::new().await;
    db.seed_feed(GUILD_S, "feed-own", "rss").await;
    db.seed_feed(GUILD_S, "feed-paused", "rss").await;
    sqlx::query("UPDATE feed_relays SET enabled = false WHERE id = $1")
        .bind("feed-paused")
        .execute(&db.pool)
        .await
        .expect("pause seeded relay");
    db.seed_feed(FOREIGN_GUILD, "feed-foreign", "twitch").await;
    let rows_before = db.feed_rows().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    // Disabled/paused relays stay inspectable; absent and foreign-guild
    // ids keep sharing the one non-oracle refusal.
    runtime
        .on_interaction(&inspect(vec![string_id("feed-paused")]))
        .await;
    runtime
        .on_interaction(&inspect(vec![string_id("feed-foreign")]))
        .await;
    runtime
        .on_interaction(&inspect(vec![string_id("feed-missing")]))
        .await;
    // Absent page and explicit page 1 render the same first page; an
    // out-of-range page keeps its navigation refusal.
    runtime.on_interaction(&list(Vec::new())).await;
    runtime
        .on_interaction(&list(vec![option("page", CommandOptionValue::Integer(1))]))
        .await;
    runtime
        .on_interaction(&list(vec![option("page", CommandOptionValue::Integer(99))]))
        .await;

    let contents = patch_contents(&mock);
    assert_eq!(contents.len(), 6, "one completion per invocation");
    assert!(
        contents[0].contains("`feed-paused`") && contents[0].contains("disabled (paused)"),
        "paused relay still inspects with its state: {:?}",
        contents[0]
    );
    assert_eq!(contents[1], "No feed relay with that id.", "foreign id");
    assert_eq!(contents[2], "No feed relay with that id.", "absent id");
    assert_eq!(contents[3], contents[4], "absent page defaults to page 1");
    assert!(
        contents[3].contains("feed-own")
            && contents[3].contains("feed-paused")
            && !contents[3].contains("feed-foreign"),
        "first page lists own relays only: {:?}",
        contents[3]
    );
    assert!(
        contents[5].contains("out of range") && contents[5].contains("/feed-list"),
        "upper-bound page keeps its refusal: {:?}",
        contents[5]
    );
    assert_eq!(
        db.feed_rows().await,
        rows_before,
        "valid reads leave saved relays unchanged"
    );
    assert!(
        db.feed_audits().await.is_empty(),
        "valid reads stay unaudited"
    );
    mock.shutdown().await;
    db.close().await;
}
