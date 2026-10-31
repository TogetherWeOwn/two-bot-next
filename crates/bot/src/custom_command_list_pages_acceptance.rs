//! Protected acceptance: real gateway dispatch -> option decoder -> store ->
//! renderer -> loopback REST. Only a disposable test database is used. These
//! tests run in the existing unit/binary CI lane, without new ignored targets.

#![cfg(test)]

use super::*;
use std::collections::HashMap;
use two_bot_core::custom_command_store;
use two_bot_core::custom_commands::StoredCommand;
use two_bot_testsupport::TestDatabase;

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable test bootstrap");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database; no application credential fallback")
}

async fn seed(pool: &PgPool, guild: &str, name: &str, trigger: Option<String>, enabled: bool) {
    custom_command_store::put_command(
        pool,
        &StoredCommand {
            guild_id: guild.to_owned(),
            name: name.to_owned(),
            description: "List fixture".to_owned(),
            template: "@everyone 😀".to_owned(),
            text_trigger: trigger,
            enabled,
        },
        "88",
        "88",
        "2026-10-01T00:00:00Z",
        "2026-10-01T00:00:00Z",
    )
    .await
    .expect("seed real command store");
}

async fn state(pool: &PgPool) -> (String, i64) {
    let commands = sqlx::query_scalar::<_, String>(
        "SELECT COALESCE(jsonb_agg(to_jsonb(c) ORDER BY guild_id, name), '[]'::jsonb)::text FROM automation_commands c",
    )
    .fetch_one(pool).await.expect("snapshot all command fields, including timestamps");
    let audits = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM automation_audit_log")
        .fetch_one(pool)
        .await
        .expect("read-only audit count");
    (commands, audits)
}

async fn dispatch(pool: PgPool, interaction: Interaction, automations: bool) -> Vec<RestRequest> {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(gates(automations, false)),
        GUILD,
        automations,
    );
    let guard = runtime.dispatch_guard();
    runtime.initialize_custom_commands_for_test(
        crate::gateway_commands::GatewayCommandConfig::from_map_for_activation(
            GUILD,
            &HashMap::from([(
                "TWO_AUTOMATIONS".to_owned(),
                if automations { "1" } else { "0" }.to_owned(),
            )]),
            false,
        )
        .expect("test gateway configuration"),
    );
    assert!(
        runtime.dispatch(&interaction_event(interaction)),
        "gateway admits request"
    );
    wait_for(
        || (0..6).all(|lane| runtime.lane_in_flight(lane) == 0),
        "command-list gateway work settles",
    )
    .await;
    let requests = mock.requests();
    drop(guard);
    drop(runtime);
    mock.shutdown().await;
    requests
}

fn interaction(options: Vec<CommandDataOption>) -> Interaction {
    slash("command-list", Some(CHANNEL), options)
}

fn page(value: i64) -> Vec<CommandDataOption> {
    vec![option("page", CommandOptionValue::Integer(value))]
}

fn reply(requests: &[RestRequest]) -> String {
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|r| r.path.ends_with("/callback"))
        .collect();
    assert_eq!(
        callbacks.len(),
        1,
        "one initial acknowledgement: {requests:?}"
    );
    assert_eq!(callbacks[0].method, "POST");
    let initial: serde_json::Value =
        serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
    assert_eq!(
        initial["data"]["flags"], 64,
        "all list results are ephemeral"
    );
    let body = if initial["type"] == 5 {
        assert_eq!(
            requests.len(),
            2,
            "defer + completion, no mutation or second reply"
        );
        assert!(requests[0].path.ends_with("/callback"), "defer comes first");
        assert_eq!(requests[1].method, "PATCH");
        assert!(requests[1].path.ends_with("/messages/@original"));
        serde_json::from_slice::<serde_json::Value>(&requests[1].body).expect("completion JSON")
    } else {
        assert_eq!(initial["type"], 4);
        assert_eq!(requests.len(), 1, "immediate refusal is the only response");
        initial["data"].clone()
    };
    assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));
    let content = body["content"].as_str().expect("human-readable list reply");
    assert!(
        content.encode_utf16().count() <= 2000,
        "entire response budget"
    );
    assert!(
        !content.contains("@everyone"),
        "templates are not list rows"
    );
    content.to_owned()
}

fn metadata(content: &str) -> Option<(usize, usize)> {
    let tail = content.split_once("Page ")?.1;
    let (current, total) = tail.split_once(" of ")?;
    let total: String = total.chars().take_while(char::is_ascii_digit).collect();
    Some((current.parse().ok()?, total.parse().ok()?))
}

fn rows(content: &str) -> Vec<&str> {
    content
        .lines()
        .filter(|line| line.starts_with('/'))
        .collect()
}

fn refusal(content: &str) {
    assert!(
        content.encode_utf16().count() <= 300,
        "short refusal: {content}"
    );
    assert!(
        content.to_ascii_lowercase().contains("page"),
        "explain bad page: {content}"
    );
    assert!(
        content.contains("/command-list"),
        "name the remedy: {content}"
    );
    assert!(
        content.contains('1'),
        "give a valid starting page or range: {content}"
    );
    assert!(
        !content.contains(" — "),
        "bad page must not silently serve rows: {content}"
    );
    assert!(
        !content.contains("No custom commands defined."),
        "out-of-range is not empty success"
    );
}

#[tokio::test]
async fn custom_list_acceptance_gateway_covers_every_maximum_row_exactly_once() {
    let db = database().await;
    // Reverse insertion order catches accidental insertion-order pagination.
    for index in (0..49).rev() {
        let name = format!("c{index:02}{}", "x".repeat(29));
        seed(
            db.pool(),
            GUILD_S,
            &name,
            Some(format!("!{name}")),
            index % 2 == 0,
        )
        .await;
    }
    seed(
        db.pool(),
        "9999",
        "foreign",
        Some("!foreign".to_owned()),
        true,
    )
    .await;
    let before = state(db.pool()).await;
    let default = dispatch(db.pool().clone(), interaction(Vec::new()), true).await;
    let first = dispatch(db.pool().clone(), interaction(page(1)), true).await;
    // Collect within a hard test-side ceiling even if production metadata is
    // broken. Assertions run after explicit teardown, including on baseline RED.
    let first_json: serde_json::Value =
        serde_json::from_slice(&first.last().expect("reply").body).expect("JSON");
    let first_content = first_json["content"].as_str().unwrap_or("");
    let total = metadata(first_content)
        .map(|(_, total)| total)
        .unwrap_or(1)
        .clamp(1, 49);
    let mut pages = vec![first];
    for number in 2..=total {
        pages.push(dispatch(db.pool().clone(), interaction(page(number as i64)), true).await);
    }
    let last_again = dispatch(db.pool().clone(), interaction(page(total as i64)), true).await;
    let past_end = dispatch(db.pool().clone(), interaction(page(total as i64 + 1)), true).await;
    let after = state(db.pool()).await;
    db.close().await.expect("drop only owned fixture");

    assert_eq!(
        before, after,
        "list and refusal neither mutate commands nor write audits"
    );
    let content: Vec<_> = pages.iter().map(|requests| reply(requests)).collect();
    assert_eq!(
        reply(&default),
        content[0],
        "omitted page is byte-identical to page 1"
    );
    assert_eq!(
        metadata(&content[0]),
        Some((1, total)),
        "page/total must be visible"
    );
    assert!(
        total > 1,
        "49 maximal legal rows cannot fit in one Discord reply"
    );
    let expected: Vec<_> = (0..49)
        .map(|index| {
            let suffix = "x".repeat(29);
            let state = if index % 2 == 0 { "on" } else { "off" };
            format!("/c{index:02}{suffix} (or !c{index:02}{suffix}) — {state}")
        })
        .collect();
    let mut covered = Vec::new();
    for (index, text) in content.iter().enumerate() {
        assert_eq!(
            metadata(text),
            Some((index + 1, total)),
            "stable page totals"
        );
        let lines = rows(text);
        assert!(!lines.is_empty(), "no empty intermediate page");
        covered.extend(lines.into_iter().map(str::to_owned));
        if index + 1 < total {
            assert!(
                text.contains(&format!("/command-list page:{}", index + 2)),
                "actionable next page: {text}"
            );
        } else {
            assert!(
                !text.contains(&format!("/command-list page:{}", total + 1)),
                "no nonexistent next page"
            );
        }
        assert!(!text.contains("foreign"), "guild-scoped read");
    }
    assert_eq!(
        covered, expected,
        "stable order; every enabled/disabled row and trigger exactly once"
    );
    assert_eq!(
        reply(&last_again),
        *content.last().expect("last page"),
        "selection is deterministic"
    );
    refusal(&reply(&past_end));
}

#[tokio::test]
async fn custom_list_acceptance_gateway_rejects_invalid_pages_without_mutation() {
    let db = database().await;
    seed(db.pool(), GUILD_S, "alpha", None, false).await;
    let before = state(db.pool()).await;
    let disabled_only = dispatch(db.pool().clone(), interaction(Vec::new()), true).await;
    let invalid = vec![
        page(0),
        page(-1),
        page(i64::MIN),
        page(i64::MAX),
        page(50),
        vec![option("page", CommandOptionValue::String("2".to_owned()))],
        vec![option(
            "page",
            CommandOptionValue::String("@everyone invalid-page".to_owned()),
        )],
        vec![option("page", CommandOptionValue::Number(1.5))],
        vec![option("page", CommandOptionValue::Boolean(true))],
        vec![option("page", CommandOptionValue::SubCommand(Vec::new()))],
        vec![
            option("page", CommandOptionValue::Integer(1)),
            option("page", CommandOptionValue::Integer(2)),
        ],
    ];
    let mut results = Vec::new();
    for options in invalid {
        results.push(dispatch(db.pool().clone(), interaction(options), true).await);
    }
    let after = state(db.pool()).await;
    db.close().await.expect("drop own invalid-page fixture");
    assert_eq!(
        before, after,
        "refusals must not mutate rows, timestamps or audit"
    );
    assert_eq!(
        rows(&reply(&disabled_only)),
        ["/alpha — off"],
        "all-disabled guild is still discoverable"
    );
    for result in results {
        let content = reply(&result);
        refusal(&content);
        assert!(
            !content.contains("invalid-page"),
            "never echo malformed input"
        );
    }
}

#[tokio::test]
async fn custom_list_acceptance_gateway_empty_and_small_sets_preserve_discovery() {
    let db = database().await;
    let empty_default = dispatch(db.pool().clone(), interaction(Vec::new()), true).await;
    let empty_first = dispatch(db.pool().clone(), interaction(page(1)), true).await;
    let empty_second = dispatch(db.pool().clone(), interaction(page(2)), true).await;
    seed(db.pool(), GUILD_S, "beta", Some("!b".to_owned()), true).await;
    seed(db.pool(), GUILD_S, "alpha", None, false).await;
    let before = state(db.pool()).await;
    let small = dispatch(db.pool().clone(), interaction(Vec::new()), true).await;
    let small_first = dispatch(db.pool().clone(), interaction(page(1)), true).await;
    let small_second = dispatch(db.pool().clone(), interaction(page(2)), true).await;
    let after = state(db.pool()).await;
    db.close().await.expect("drop own empty/small fixture");
    assert_eq!(before, after);
    assert_eq!(before.1, 0, "empty-list operations did not audit");
    assert_eq!(reply(&empty_default), "No custom commands defined.");
    assert_eq!(reply(&empty_first), "No custom commands defined.");
    refusal(&reply(&empty_second));
    let content = reply(&small);
    assert_eq!(reply(&small_first), content);
    assert_eq!(rows(&content), ["/alpha — off", "/beta (or !b) — on"]);
    assert_eq!(metadata(&content), Some((1, 1)));
    refusal(&reply(&small_second));
}

#[tokio::test]
async fn custom_list_acceptance_gateway_keeps_authorization_and_scope_before_store() {
    for (automations, permissions, expected) in [
        (true, Permissions::empty(), MANAGE_SERVER_REQUIRED),
        (false, Permissions::MANAGE_GUILD, AUTOMATIONS_DISABLED_REPLY),
    ] {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
            .expect("never-used pool; refusal precedes storage");
        let denied = from_member(interaction(page(0)), 88, permissions);
        let result = dispatch(pool, denied, automations).await;
        assert_eq!(
            reply(&result),
            expected,
            "page validation cannot bypass authorization"
        );
    }
    for wrong_application in [false, true] {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
            .expect("never-used pool");
        let mut foreign = interaction(page(2));
        if wrong_application {
            foreign.application_id = Id::new(9999);
        } else {
            foreign.guild_id = Some(Id::new(9999));
        }
        assert!(
            dispatch(pool, foreign, true).await.is_empty(),
            "foreign scope stays silent"
        );
    }
}
