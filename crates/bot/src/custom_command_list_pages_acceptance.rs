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

// Prefix coverage uses the same real gateway/store/renderer as the existing
// unfiltered controls above. Only Discord HTTP is replaced by loopback REST.
fn prefix_options(prefix: &str, selected_page: Option<i64>) -> Vec<CommandDataOption> {
    let mut options = vec![option(
        "name-prefix",
        CommandOptionValue::String(prefix.to_owned()),
    )];
    if let Some(value) = selected_page {
        options.extend(page(value));
    }
    options
}

async fn prefix_database() -> TestDatabase {
    let db = database().await;
    // A nonempty audit baseline catches deletion/update as well as new run or
    // mutation records; counting an empty log alone would miss replacement.
    sqlx::query("INSERT INTO automation_audit_log (id, guild_id, actor_id, action, target_key, outcome, reason, created_at) VALUES ('prefix-control', '2222', '88', 'command_put', 'existing', 'ok', NULL, '2026-10-01T00:00:00Z')")
        .execute(db.pool()).await.expect("seed persistent audit control");
    db
}

async fn prefix_state(pool: &PgPool) -> (String, String) {
    let commands = state(pool).await.0;
    let audits = sqlx::query_scalar::<_, String>(
        "SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY id), '[]'::jsonb)::text FROM automation_audit_log a",
    ).fetch_one(pool).await.expect("snapshot every audit field");
    (commands, audits)
}

// All options still reach production decoding. Count acquisitions on a pool
// built from the guarded fixture's options, not a mock store that can prefilter.
async fn counted_pool(db: &TestDatabase) -> (PgPool, Arc<AtomicU64>) {
    let reads = Arc::new(AtomicU64::new(0));
    let observed = reads.clone();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .after_acquire(move |_, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(true) })
        })
        .connect_with(db.pool().connect_options().as_ref().clone())
        .await
        .expect("independent guarded fixture pool");
    reads.store(0, Ordering::SeqCst);
    (pool, reads)
}

fn raw_content(requests: &[RestRequest]) -> String {
    // Navigation discovery must not panic before explicit fixture teardown.
    requests
        .last()
        .and_then(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
        .and_then(|v| {
            v["content"]
                .as_str()
                .or_else(|| v["data"]["content"].as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

fn prefix_summary(content: &str, prefix: &str, count: usize) {
    let summary = content
        .lines()
        .filter(|line| !line.starts_with('/'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        summary.contains(prefix),
        "show normalized active prefix: {content}"
    );
    // Wording/layout is free, including a total beside the page metadata.
    // Remove just the page numbers and navigation before looking for a total.
    let summary = match metadata(content) {
        Some((page, total)) => summary.replace(&format!("Page {page} of {total}"), ""),
        None => summary,
    };
    let without_pages = summary
        .lines()
        .map(|line| {
            line.split_once("/command-list")
                .map_or(line, |(head, _)| head)
        })
        .collect::<Vec<_>>()
        .join(" ");
    let numbers: Vec<_> = without_pages
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse::<usize>().ok())
        .collect();
    assert!(
        numbers.contains(&count),
        "show filtered total {count}: {content}"
    );
}

fn next_prefix_options(content: &str) -> Option<Vec<CommandDataOption>> {
    let invocation = content
        .lines()
        .find_map(|line| line.split_once("/command-list ").map(|(_, tail)| tail))?;
    let mut result = Vec::new();
    for token in invocation.split_whitespace() {
        let (name, value) = token.trim_matches('`').split_once(':')?;
        match name {
            "page" => result.push(option(
                name,
                CommandOptionValue::Integer(value.parse().ok()?),
            )),
            "name-prefix" => {
                result.push(option(name, CommandOptionValue::String(value.to_owned())))
            }
            _ => return None,
        }
    }
    Some(result)
}

fn prefix_refusal(content: &str) {
    assert!(
        content.encode_utf16().count() <= 300,
        "bounded correction: {content}"
    );
    assert!(
        content.to_ascii_lowercase().contains("prefix"),
        "explain invalid prefix: {content}"
    );
    assert!(
        content.contains("/command-list"),
        "copyable remedy: {content}"
    );
    assert!(
        rows(content).is_empty(),
        "invalid input must not silently list: {content}"
    );
    assert!(
        !content.contains("unavailable"),
        "validation precedes store: {content}"
    );
    assert!(
        !content.contains("invalid-filter"),
        "never echo hostile input: {content}"
    );
}

#[tokio::test]
async fn custom_prefix_acceptance_filters_complete_snapshot_before_paging_and_replays_hint() {
    let db = prefix_database().await;
    // 49 local rows: 30 maximal matching names plus 19 distractors. Insert
    // interleaved/reversed; sorted distractors span both sides of the matches.
    for index in (0..30).rev() {
        let name = format!("m{index:02}{}", "x".repeat(29));
        seed(
            db.pool(),
            GUILD_S,
            &name,
            Some(format!("!{name}")),
            index % 2 == 0,
        )
        .await;
        if index < 19 {
            let other = if index % 2 == 0 {
                format!("a{index:02}")
            } else {
                format!("z{index:02}")
            };
            seed(db.pool(), GUILD_S, &other, None, true).await;
        }
    }
    seed(
        db.pool(),
        "9999",
        "mforeign",
        Some("!mforeign".to_owned()),
        true,
    )
    .await;
    let before = prefix_state(db.pool()).await;
    let first = dispatch(
        db.pool().clone(),
        interaction(prefix_options("m", None)),
        true,
    )
    .await;
    let explicit = dispatch(
        db.pool().clone(),
        interaction(prefix_options("m", Some(1))),
        true,
    )
    .await;
    let uppercase = dispatch(
        db.pool().clone(),
        interaction(prefix_options("M", Some(1))),
        true,
    )
    .await;
    let total = metadata(&raw_content(&first))
        .map(|(_, total)| total)
        .unwrap_or(1)
        .clamp(1, 49);
    let mut pages = vec![first];
    let mut replayed = Vec::new();
    for number in 2..=total {
        let previous = raw_content(pages.last().expect("first page exists"));
        if let Some(options) = next_prefix_options(&previous) {
            replayed.push(dispatch(db.pool().clone(), interaction(options), true).await);
        }
        pages.push(
            dispatch(
                db.pool().clone(),
                interaction(prefix_options("m", Some(number as i64))),
                true,
            )
            .await,
        );
    }
    let last_again = dispatch(
        db.pool().clone(),
        interaction(prefix_options("m", Some(total as i64))),
        true,
    )
    .await;
    let past_end = dispatch(
        db.pool().clone(),
        interaction(prefix_options("m", Some(total as i64 + 1))),
        true,
    )
    .await;
    let huge = dispatch(
        db.pool().clone(),
        interaction(prefix_options("m", Some(i64::MAX))),
        true,
    )
    .await;
    let after = prefix_state(db.pool()).await;
    db.close()
        .await
        .expect("drop own prefix paging fixture before assertions");

    assert_eq!(
        before, after,
        "byte-identical command/audit rows, no run records"
    );
    let content: Vec<_> = pages.iter().map(|r| reply(r)).collect();
    assert!(total > 1, "30 maximal rows require whole-row paging");
    assert_eq!(
        reply(&explicit),
        content[0],
        "omitted page remains page one"
    );
    assert_eq!(
        reply(&uppercase),
        content[0],
        "normalize uppercase before filtering and display"
    );
    let expected: Vec<_> = (0..30)
        .map(|index| {
            let suffix = "x".repeat(29);
            let enabled = if index % 2 == 0 { "on" } else { "off" };
            format!("/m{index:02}{suffix} (or !m{index:02}{suffix}) — {enabled}")
        })
        .collect();
    let mut actual = Vec::new();
    for (index, text) in content.iter().enumerate() {
        assert_eq!(metadata(text), Some((index + 1, total)));
        prefix_summary(text, "m", 30);
        let lines = rows(text);
        assert!(!lines.is_empty(), "no empty intermediate page");
        actual.extend(lines.into_iter().map(str::to_owned));
        assert!(!text.contains("mforeign"), "never another guild");
        if index + 1 < total {
            let hint = next_prefix_options(text).expect("copyable next-page invocation");
            assert_eq!(hint.len(), 2, "next page retains exactly page and filter");
            assert!(hint.iter().any(|o| o.name == "name-prefix"
                && o.value == CommandOptionValue::String("m".to_owned())));
            assert!(hint
                .iter()
                .any(|o| o.name == "page"
                    && o.value == CommandOptionValue::Integer(index as i64 + 2)));
        } else {
            assert!(next_prefix_options(text).is_none(), "no phantom next page");
        }
    }
    assert_eq!(
        actual, expected,
        "filter before paging; sorted complete rows including disabled"
    );
    assert_eq!(
        replayed.len(),
        total - 1,
        "every navigation hint can be dispatched"
    );
    for (index, requests) in replayed.iter().enumerate() {
        assert_eq!(
            reply(requests),
            content[index + 1],
            "copied hint selects same filtered page"
        );
    }
    assert_eq!(reply(&last_again), *content.last().expect("last"));
    refusal(&reply(&past_end));
    refusal(&reply(&huge));
}

#[tokio::test]
async fn custom_prefix_acceptance_matches_only_name_start_and_keeps_disabled_row_fields() {
    let db = prefix_database().await;
    for (name, trigger, enabled) in [
        ("faq", None, true),
        ("faq-help", Some("!answer".to_owned()), false),
        ("pre-faq", None, true),
        ("unrelated", Some("!faq".to_owned()), true),
    ] {
        seed(db.pool(), GUILD_S, name, trigger, enabled).await;
    }
    sqlx::query("UPDATE automation_commands SET description = 'faq', template = 'faq @everyone 😀' WHERE name = 'unrelated'")
        .execute(db.pool()).await.expect("body/description/trigger decoys");
    seed(db.pool(), "9999", "faq-foreign", None, true).await;
    let before = prefix_state(db.pool()).await;
    let filtered = dispatch(
        db.pool().clone(),
        interaction(prefix_options("FAQ", None)),
        true,
    )
    .await;
    let boundary = dispatch(
        db.pool().clone(),
        interaction(prefix_options("faq", Some(2))),
        true,
    )
    .await;
    let after = prefix_state(db.pool()).await;
    db.close().await.expect("drop name-only fixture");
    assert_eq!(before, after);
    let content = reply(&filtered);
    assert_eq!(
        rows(&content),
        ["/faq — on", "/faq-help (or !answer) — off"]
    );
    assert_eq!(metadata(&content), Some((1, 1)));
    prefix_summary(&content, "faq", 2);
    assert!(!content.contains("FAQ"), "display normalized prefix");
    assert!(!content.contains("foreign"));
    assert!(next_prefix_options(&content).is_none());
    refusal(&reply(&boundary));
}

#[tokio::test]
async fn custom_prefix_acceptance_accepts_full_name_alphabet_and_maximum_length() {
    let db = prefix_database().await;
    let maximum = "a".repeat(32);
    for name in [
        maximum.as_str(),
        "0zero",
        "-dash",
        "_under",
        "helpful",
        "unrelated",
    ] {
        seed(db.pool(), GUILD_S, name, None, false).await;
    }
    let before = prefix_state(db.pool()).await;
    let mut results = Vec::new();
    for (prefix, expected) in [
        (maximum.to_uppercase(), format!("/{maximum} — off")),
        ("0".to_owned(), "/0zero — off".to_owned()),
        ("-".to_owned(), "/-dash — off".to_owned()),
        ("_".to_owned(), "/_under — off".to_owned()),
        ("HELP".to_owned(), "/helpful — off".to_owned()),
    ] {
        let requests = dispatch(
            db.pool().clone(),
            interaction(prefix_options(&prefix, None)),
            true,
        )
        .await;
        results.push((prefix, expected, requests));
    }
    let after = prefix_state(db.pool()).await;
    db.close().await.expect("drop alphabet fixture");
    assert_eq!(before, after);
    for (prefix, expected, requests) in results {
        let content = reply(&requests);
        assert_eq!(
            rows(&content),
            [expected],
            "valid prefix {prefix}; builtin prefixes are not reserved names"
        );
        prefix_summary(&content, &prefix.to_ascii_lowercase(), 1);
        assert_eq!(metadata(&content), Some((1, 1)));
    }
}

#[tokio::test]
async fn custom_prefix_acceptance_distinguishes_no_names_empty_guild_and_filtered_page_miss() {
    let db = prefix_database().await;
    let empty = dispatch(
        db.pool().clone(),
        interaction(prefix_options("zzz", None)),
        true,
    )
    .await;
    let empty_plain = dispatch(db.pool().clone(), interaction(Vec::new()), true).await;
    seed(db.pool(), GUILD_S, "alpha", None, false).await;
    seed(db.pool(), "9999", "zzz-foreign", None, true).await;
    let before = prefix_state(db.pool()).await;
    let none = dispatch(
        db.pool().clone(),
        interaction(prefix_options("ZZZ", None)),
        true,
    )
    .await;
    let none_page_two = dispatch(
        db.pool().clone(),
        interaction(prefix_options("zzz", Some(2))),
        true,
    )
    .await;
    let hit_page_two = dispatch(
        db.pool().clone(),
        interaction(prefix_options("alpha", Some(2))),
        true,
    )
    .await;
    let after = prefix_state(db.pool()).await;
    db.close().await.expect("drop empty/miss fixture");
    assert_eq!(before, after);
    let empty = reply(&empty);
    let none = reply(&none);
    assert_eq!(reply(&empty_plain), "No custom commands defined.");
    assert!(
        empty.to_ascii_lowercase().contains("no custom commands"),
        "empty guild is distinct: {empty}"
    );
    assert_ne!(empty, none);
    let lower = none.to_ascii_lowercase();
    assert!(
        lower.contains("no") && lower.contains("match"),
        "explain no matching names: {none}"
    );
    assert!(
        none.contains("zzz") && none.contains("/command-list"),
        "normalized filter and recovery: {none}"
    );
    assert!(rows(&none).is_empty());
    assert!(!none.contains("foreign"));
    assert!(!lower.contains("unavailable"));
    assert_ne!(none, reply(&none_page_two));
    refusal(&reply(&none_page_two));
    refusal(&reply(&hit_page_two));
}

#[tokio::test]
async fn custom_prefix_acceptance_rejects_invalid_and_duplicate_filters_before_any_store_read() {
    let db = prefix_database().await;
    seed(db.pool(), GUILD_S, "alpha", None, true).await;
    let (pool, reads) = counted_pool(&db).await;
    let probe = dispatch(pool.clone(), interaction(Vec::new()), true).await;
    let live_reads = reads.swap(0, Ordering::SeqCst);
    let before = prefix_state(db.pool()).await;
    let mut invalid: Vec<_> = [
        "",
        " ",
        " alpha",
        "alpha ",
        "a b",
        "/alpha",
        "!alpha",
        "alpha*",
        "alpha%",
        "a.b",
        "a\nb",
        "😀",
        "аlpha",
        "@everyone invalid-filter",
        "'; DROP TABLE invalid-filter; --",
    ]
    .into_iter()
    .map(|s| prefix_options(s, None))
    .collect();
    invalid.push(prefix_options(&"a".repeat(33), None));
    for value in [
        CommandOptionValue::Integer(1),
        CommandOptionValue::Number(1.5),
        CommandOptionValue::Boolean(true),
        CommandOptionValue::SubCommand(Vec::new()),
    ] {
        invalid.push(vec![option("name-prefix", value)]);
    }
    for second in ["alpha", "beta"] {
        let mut duplicate = prefix_options("alpha", None);
        duplicate.extend(prefix_options(second, None));
        invalid.push(duplicate);
    }
    let mut results = Vec::new();
    for options in invalid {
        let result = dispatch(pool.clone(), interaction(options), true).await;
        results.push((result, reads.swap(0, Ordering::SeqCst)));
    }
    let after = prefix_state(db.pool()).await;
    pool.close().await;
    db.close()
        .await
        .expect("drop invalid filter fixture before assertions");
    assert!(
        live_reads > 0,
        "acquisition witness observes real successful store reads"
    );
    assert_eq!(rows(&reply(&probe)), ["/alpha — on"]);
    assert_eq!(before, after);
    for (index, (requests, reads)) in results.iter().enumerate() {
        let content = reply(requests);
        assert_eq!(
            *reads, 0,
            "case {index}: invalid prefix must be refused before any store read; {content}"
        );
        prefix_refusal(&content);
    }
}

#[tokio::test]
async fn custom_prefix_acceptance_bad_pages_with_filter_do_not_reach_store() {
    let db = prefix_database().await;
    seed(db.pool(), GUILD_S, "alpha", None, true).await;
    let (pool, reads) = counted_pool(&db).await;
    let probe = dispatch(pool.clone(), interaction(Vec::new()), true).await;
    let live_reads = reads.swap(0, Ordering::SeqCst);
    let before = prefix_state(db.pool()).await;
    let mut invalid = vec![page(0), page(-1), page(i64::MIN)];
    for value in [
        CommandOptionValue::String("@everyone invalid-page".to_owned()),
        CommandOptionValue::Number(1.5),
        CommandOptionValue::Boolean(true),
        CommandOptionValue::SubCommand(Vec::new()),
    ] {
        invalid.push(vec![option("page", value)]);
    }
    for second in [1, 2] {
        invalid.push(vec![
            option("page", CommandOptionValue::Integer(1)),
            option("page", CommandOptionValue::Integer(second)),
        ]);
    }
    let mut results = Vec::new();
    for mut options in invalid {
        options.extend(prefix_options("alpha", None));
        results.push((
            dispatch(pool.clone(), interaction(options), true).await,
            reads.swap(0, Ordering::SeqCst),
        ));
    }
    let after = prefix_state(db.pool()).await;
    pool.close().await;
    db.close().await.expect("drop invalid page/filter fixture");
    assert!(live_reads > 0);
    assert_eq!(rows(&reply(&probe)), ["/alpha — on"]);
    assert_eq!(before, after);
    for (requests, reads) in results {
        assert_eq!(reads, 0, "bad page validation before store");
        let content = reply(&requests);
        refusal(&content);
        assert!(!content.contains("invalid-page"));
    }
}

#[tokio::test]
async fn custom_prefix_acceptance_preserves_feature_admin_guild_application_and_dm_admission() {
    let db = prefix_database().await;
    let (pool, reads) = counted_pool(&db).await;
    let before = prefix_state(db.pool()).await;
    let mut denied = Vec::new();
    for (enabled, permissions, expected) in [
        (true, Permissions::empty(), MANAGE_SERVER_REQUIRED),
        (false, Permissions::MANAGE_GUILD, AUTOMATIONS_DISABLED_REPLY),
    ] {
        let request = from_member(
            interaction(prefix_options("@everyone invalid-filter", None)),
            88,
            permissions,
        );
        denied.push((dispatch(pool.clone(), request, enabled).await, expected));
    }
    let mut silent = Vec::new();
    for scope in 0..3 {
        let mut request = interaction(prefix_options("alpha", None));
        match scope {
            0 => request.guild_id = Some(Id::new(9999)),
            1 => request.application_id = Id::new(9999),
            _ => request.guild_id = None,
        }
        silent.push(dispatch(pool.clone(), request, true).await);
    }
    let acquisitions = reads.load(Ordering::SeqCst);
    let after = prefix_state(db.pool()).await;
    pool.close().await;
    db.close().await.expect("drop admission fixture");
    assert_eq!(before, after);
    assert_eq!(acquisitions, 0, "admission cannot read store");
    for (requests, expected) in denied {
        assert_eq!(reply(&requests), expected);
    }
    for requests in silent {
        assert!(
            requests.is_empty(),
            "foreign scope/DM stays silent: {requests:?}"
        );
    }
}

#[tokio::test]
async fn custom_prefix_acceptance_closed_database_is_unavailable_not_no_matching_names() {
    let db = prefix_database().await;
    seed(db.pool(), GUILD_S, "alpha", None, true).await;
    let before = prefix_state(db.pool()).await;
    let closed = db
        .independent_pool()
        .await
        .expect("guarded independent pool");
    closed.close().await;
    let filtered = dispatch(
        closed.clone(),
        interaction(prefix_options("ALPHA", None)),
        true,
    )
    .await;
    let unfiltered = dispatch(closed, interaction(Vec::new()), true).await;
    let after = prefix_state(db.pool()).await;
    db.close()
        .await
        .expect("drop outage fixture through healthy owner pool");
    assert_eq!(before, after);
    for requests in [filtered, unfiltered] {
        let content = reply(&requests);
        assert!(
            content.to_ascii_lowercase().contains("unavailable"),
            "honest failed read: {content}"
        );
        assert!(!content.to_ascii_lowercase().contains("no match"));
        assert!(!content.contains("No custom commands defined."));
        assert!(
            !content.contains("postgres") && !content.contains("agent_test"),
            "bounded non-credential error"
        );
        assert!(rows(&content).is_empty());
    }
}
