//! Protected behavior contract: `/command-inspect draft:<template>` previews
//! proposed text, never a saved definition. `name` and `draft` are exclusive;
//! draft alone defaults to preview, and explicit `view:preview` is equivalent.
//! Rendering keeps the existing four-variable, single-pass language. The trusted
//! first line is `Draft preview — not saved or published`; untrusted body text
//! uses the saved preview's Markdown escaping and UTF-16 framing policy.
//!
//! These tests catch accidental saved lookup, execution or publication through
//! real runtime dispatch. The bound SQL tripwire catches even swallowed storage
//! errors; the full HTTP transcript permits only the private reply. Two explicit
//! disposable-DB cases additionally check retained rows/audits, saved parity and
//! the first subsequent real prefix invocation. No implementation adapter.

use super::*;
use std::time::Duration;

const HEADING: &str = "Draft preview — not saved or published";

fn proposed(id: u64, template: &str) -> Interaction {
    interaction(id, 32, vec![option("draft", template)])
}

async fn tripwire_pool() -> (sqlx::PgPool, tokio::net::TcpListener) {
    // Retain the bound socket, rather than release a port another parallel test
    // could claim. This listener is not a database and handles no SQL protocol.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind SQL-access tripwire");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(100))
        .connect_lazy_with(
            PgConnectOptions::new()
                .host("127.0.0.1")
                .port(listener.local_addr().expect("tripwire address").port())
                .username("preview_fixture")
                .database("preview_fixture")
                .ssl_mode(sqlx::postgres::PgSslMode::Disable),
        );
    (pool, listener)
}

fn no_sql(listener: tokio::net::TcpListener) {
    let listener = listener.into_std().expect("nonblocking SQL tripwire");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "draft preview or refusal attempted storage access"
    );
}

fn private_content(observed: &Observed) -> String {
    let content = super::private_content(observed);
    for request in &observed.requests {
        let wire: Value = serde_json::from_slice(&request.body).expect("private response JSON");
        let data = if request.method == "POST" {
            &wire["data"]
        } else {
            &wire
        };
        assert!(
            data["components"].is_null() || data["components"] == json!([]),
            "preview must not expose mutation controls"
        );
    }
    content
}

fn draft_body(observed: &Observed) -> String {
    view_body(&private_content(observed), HEADING)
}

fn refusal(observed: &Observed) -> String {
    let content = private_content(observed);
    assert!(
        !content.starts_with(HEADING),
        "refusal is not a successful preview: {content}"
    );
    assert!(
        content.contains("/command-inspect"),
        "actionable inspection correction: {content}"
    );
    assert!(
        !content.contains("temporarily unavailable"),
        "input error is not a storage outage: {content}"
    );
    content
}

// Catches a renderer that uses stored text, omits a variable, uses the wrong
// actor/channel, or delegates draft to ordinary command execution.
#[tokio::test]
async fn draft_all_placeholders_render_invocation_context_without_sql_or_execution() {
    let (pool, listener) = tripwire_pool().await;
    let mock = discord().await;
    let runtime = runtime(pool.clone(), &mock, true);
    let observed = observe(
        &runtime,
        &mock,
        &proposed(700, "{user}|{username}|{server}|{channel}"),
        Some("世界"),
    )
    .await;
    no_sql(listener);
    assert_eq!(draft_body(&observed), "<@3333>\\|tester\\|世界\\|<#4444>");
    assert!(runtime.published_commands().is_none());
    pool.close().await;
    mock.shutdown().await;
}

// Existing language intentionally treats malformed/capitalized braces literally
// and substitutes the inner allowed key in double braces. Do not invent a parser.
#[tokio::test]
async fn draft_preserves_literal_malformed_and_nested_placeholder_semantics() {
    let (pool, listener) = tripwire_pool().await;
    let mock = discord().await;
    let runtime = runtime(pool.clone(), &mock, true);
    let mut observations = Vec::new();
    for (index, (template, expected)) in [
        ("{user}", "<@3333>"),
        ("{username}", "tester"),
        ("{server}", "TWO"),
        ("{channel}", "<#4444>"),
        ("{{user}}", "{<@3333>}"),
        ("{User} {} {user {user-name}", "{User} {} {user {user-name}"),
        ("😀 三\nplain", "😀 三 plain"),
    ]
    .into_iter()
    .enumerate()
    {
        observations.push((
            expected,
            observe(
                &runtime,
                &mock,
                &proposed(710 + index as u64, template),
                Some("TWO"),
            )
            .await,
        ));
    }
    no_sql(listener);
    for (expected, observed) in observations {
        assert_eq!(draft_body(&observed), expected);
    }
    pool.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn draft_context_is_single_pass_and_arguments_never_become_template_source() {
    let (pool, listener) = tripwire_pool().await;
    let mock = discord().await;
    let runtime = runtime(pool.clone(), &mock, true);
    let mut observations = Vec::new();
    for arguments in ["", "{user} @everyone !faq", &"😀".repeat(250)] {
        let mut invocation = interaction(
            730,
            32,
            vec![
                option("draft", "{username}|{server}"),
                option("arguments", arguments),
            ],
        );
        invocation
            .member
            .as_mut()
            .unwrap()
            .user
            .as_mut()
            .unwrap()
            .name = "{server}".to_owned();
        observations.push(observe(&runtime, &mock, &invocation, Some("TWO")).await);
    }
    no_sql(listener);
    for observed in observations {
        assert_eq!(draft_body(&observed), "{server}\\|TWO");
    }
    pool.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn draft_unknown_and_args_placeholders_refuse_before_rendering() {
    let (pool, listener) = tripwire_pool().await;
    let mock = discord().await;
    let runtime = runtime(pool.clone(), &mock, true);
    let mut observations = Vec::new();
    for (index, key) in ["unknown", "usre", "args"].into_iter().enumerate() {
        let template = format!("DO NOT EXECUTE {{{{{key}}}}}");
        observations.push((
            key,
            observe(
                &runtime,
                &mock,
                &proposed(740 + index as u64, &template),
                Some("TWO"),
            )
            .await,
        ));
    }
    no_sql(listener);
    for (key, observed) in observations {
        let content = refusal(&observed);
        assert!(content.contains(key), "identify unsupported key: {content}");
        assert!(
            content.to_lowercase().contains("placeholder"),
            "explain template error: {content}"
        );
        assert!(
            content.contains("draft"),
            "correct proposed text, not a stored response: {content}"
        );
        assert!(
            !content.contains("DO NOT EXECUTE"),
            "never present invalid input as output"
        );
    }
    pool.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn draft_exclusive_inputs_types_duplicates_and_unknown_options_refuse_without_sql() {
    let (pool, listener) = tripwire_pool().await;
    let mock = discord().await;
    let runtime = runtime(pool.clone(), &mock, true);
    let mut cases = vec![
        vec![],
        vec![option("name", "faq"), option("draft", "new")],
        vec![option("draft", "new"), option("name", "faq")],
        vec![option("draft", "one"), option("draft", "two")],
        vec![
            option("name", "faq"),
            option("name", "other"),
            option("draft", "new"),
        ],
        vec![
            option("draft", "new"),
            option("view", "preview"),
            option("view", "preview"),
        ],
        vec![
            option("draft", "new"),
            option("arguments", "one"),
            option("arguments", "two"),
        ],
        vec![option("draft", "new"), option("unknown", "value")],
        vec![option("draft", "new"), option("view", "execute")],
        vec![
            option("draft", "new"),
            option("arguments", &"😀".repeat(251)),
        ],
    ];
    for field in ["draft", "name", "view", "arguments"] {
        let mut options = if field == "draft" {
            vec![]
        } else {
            vec![option("draft", "new")]
        };
        options.push(json!({"name": field, "type": 4, "value": 7}));
        cases.push(options);
    }
    let mut observations = Vec::new();
    for (index, options) in cases.into_iter().enumerate() {
        observations.push(
            observe(
                &runtime,
                &mock,
                &interaction(760 + index as u64, 32, options),
                Some("TWO"),
            )
            .await,
        );
    }
    no_sql(listener);
    for observed in observations {
        refusal(&observed);
    }
    pool.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn draft_input_length_uses_utf16_and_expansion_overflow_is_not_partial_success() {
    let (pool, listener) = tripwire_pool().await;
    let mock = discord().await;
    let runtime = runtime(pool.clone(), &mock, true);
    let mut observations = Vec::new();
    for (index, template) in [String::new(), "x".repeat(2001), "😀".repeat(1001)]
        .into_iter()
        .enumerate()
    {
        observations.push(
            observe(
                &runtime,
                &mock,
                &proposed(790 + index as u64, &template),
                Some("TWO"),
            )
            .await,
        );
    }
    let mut expansion = proposed(795, &"{username}".repeat(200));
    expansion
        .member
        .as_mut()
        .unwrap()
        .user
        .as_mut()
        .unwrap()
        .name = "😀".repeat(32);
    let overflow = observe(&runtime, &mock, &expansion, Some("TWO")).await;
    no_sql(listener);
    for observed in observations {
        let content = refusal(&observed).to_lowercase();
        assert!(
            content.contains("2000") || content.contains("length"),
            "explain template bound: {content}"
        );
    }
    let content = refusal(&overflow).to_lowercase();
    assert!(
        content.contains("too long") || content.contains("shorten"),
        "actionable expansion refusal: {content}"
    );
    assert!(
        !content.contains('😀'),
        "overflow must not leak a partial successful render"
    );
    pool.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn draft_maximum_valid_unicode_and_escaping_account_for_response_framing() {
    let (pool, listener) = tripwire_pool().await;
    let mock = discord().await;
    let runtime = runtime(pool.clone(), &mock, true);
    let mut observations = Vec::new();
    for (index, template) in ["😀".repeat(1000), "x".repeat(2000), "*".repeat(1990)]
        .into_iter()
        .enumerate()
    {
        let input = proposed(800 + index as u64, &template);
        observations.push((
            template,
            observe(&runtime, &mock, &input, Some("TWO")).await,
        ));
    }
    no_sql(listener);
    for (template, observed) in observations {
        let content = private_content(&observed);
        let lower = content.to_lowercase();
        assert!(
            content.starts_with(HEADING)
                || lower.contains("too long")
                || lower.contains("cannot fit"),
            "truthful framing outcome: {content}"
        );
        let escaped = template.replace('*', "\\*");
        if !content.contains(&escaped) {
            assert!(
                lower.contains("truncated")
                    || lower.contains("too long")
                    || lower.contains("cannot fit"),
                "no silently clipped proposed text: {content}"
            );
        }
    }
    pool.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn draft_hostile_markdown_mentions_and_command_text_cannot_forge_or_execute_ui() {
    let (pool, listener) = tripwire_pool().await;
    let mock = discord().await;
    let runtime = runtime(pool.clone(), &mock, true);
    let template =
        "**FORGED** ``` @everyone <@&8888> [click](https://example.invalid) !faq {username}";
    let mut input = proposed(810, template);
    input.member.as_mut().unwrap().user.as_mut().unwrap().name = "**MEMBER** ```".to_owned();
    let observed = observe(&runtime, &mock, &input, Some("TWO")).await;
    no_sql(listener);
    let body = draft_body(&observed);
    assert!(body.contains("\\*\\*FORGED\\*\\*"));
    assert!(body.contains("\\*\\*MEMBER\\*\\* \\`\\`\\`"));
    assert!(!body.contains("```"));
    assert!(!body.contains("[click](https://example.invalid)"));
    assert!(body.contains("@everyone") && body.contains("<@&8888>") && body.contains("!faq"));
    pool.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn draft_repeated_preview_and_explicit_preview_work_with_an_unavailable_store() {
    let (pool, listener) = tripwire_pool().await;
    pool.close().await;
    let mock = discord().await;
    let runtime = runtime(pool, &mock, true);
    let first = observe(
        &runtime,
        &mock,
        &proposed(820, "Hello {username}"),
        Some("TWO"),
    )
    .await;
    let second = observe(
        &runtime.clone(),
        &mock,
        &proposed(821, "Hello {username}"),
        Some("TWO"),
    )
    .await;
    let explicit = interaction(
        822,
        32,
        vec![
            option("draft", "Hello {username}"),
            option("view", "preview"),
        ],
    );
    let explicit = observe(&runtime, &mock, &explicit, Some("TWO")).await;
    let saved = observe(
        &runtime,
        &mock,
        &inspect(823, "faq", Some("preview"), None),
        Some("TWO"),
    )
    .await;
    no_sql(listener);
    assert_eq!(draft_body(&first), "Hello tester");
    assert_eq!(draft_body(&second), "Hello tester");
    assert_eq!(draft_body(&explicit), "Hello tester");
    assert!(
        private_content(&saved).contains("temporarily unavailable"),
        "saved mode still needs a lookup"
    );
    mock.shutdown().await;
}

// Unchanged positive/negative controls: both authority gates precede validation,
// and foreign events remain unowned. They must pass on the named RED baseline.
#[tokio::test]
async fn draft_control_staff_and_activation_gates_precede_validation_and_storage() {
    for (enabled, permissions, expected) in [
        (true, 0, "Manage Server"),
        (true, 8, "Manage Server"),
        (false, 32, "Automations are disabled"),
    ] {
        let (pool, listener) = tripwire_pool().await;
        let mock = discord().await;
        let runtime = runtime(pool.clone(), &mock, enabled);
        let observed = observe(
            &runtime,
            &mock,
            &interaction(830, permissions, vec![option("draft", "{unknown}")]),
            Some("TWO"),
        )
        .await;
        no_sql(listener);
        assert!(private_content(&observed).contains(expected));
        pool.close().await;
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn draft_control_foreign_guild_dm_and_application_are_unowned_and_silent() {
    let (pool, listener) = tripwire_pool().await;
    let mock = discord().await;
    let runtime = runtime(pool.clone(), &mock, true);
    for scope in ["guild", "dm", "application"] {
        let mut input = proposed(840, "Hello {username}");
        match scope {
            "guild" => input.guild_id = Some(Id::new(7777)),
            "dm" => input.guild_id = None,
            "application" => input.application_id = Id::new(9999),
            _ => unreachable!(),
        }
        let observed = observe(&runtime, &mock, &input, Some("TWO")).await;
        assert!(matches!(observed.result, Ok(false)));
        assert!(observed.requests.is_empty());
    }
    no_sql(listener);
    pool.close().await;
    mock.shutdown().await;
}

#[tokio::test]
async fn draft_missing_context_refuses_truthfully_without_storage_or_template_blame() {
    let (pool, listener) = tripwire_pool().await;
    let mock = discord().await;
    let runtime = runtime(pool.clone(), &mock, true);
    let missing_guild = observe(&runtime, &mock, &proposed(850, "Hello {server}"), None).await;
    let mut input = proposed(851, "Hello {channel}");
    input.channel = None;
    let missing_channel = observe(&runtime, &mock, &input, Some("TWO")).await;
    no_sql(listener);
    for observed in [missing_guild, missing_channel] {
        let content = private_content(&observed);
        assert!(
            content.to_lowercase().contains("context"),
            "not an invalid-template or missing-name refusal: {content}"
        );
        assert!(!content.starts_with(HEADING));
        assert!(!content.contains("Replace") && !content.contains("temporarily unavailable"));
    }
    pool.close().await;
    mock.shutdown().await;
}

// Real persistence control: a preview is not an edit or enable operation, even
// with disabled saved rows present. Both render paths must agree on fixed input.
#[tokio::test]
async fn draft_db_preserves_disabled_saved_definition_and_all_audits_with_saved_renderer_parity() {
    let db = database().await;
    let template = "{user}|{username}|{server}|{channel}|{{user}}";
    seed(db.pool(), "2222", "faq", template, false).await;
    seed(db.pool(), "7777", "faq", "FOREIGN SECRET", true).await;
    let before = definitions(db.pool()).await;
    let mock = discord().await;
    let runtime = runtime(db.pool().clone(), &mock, true);
    let draft = observe(&runtime, &mock, &proposed(860, template), Some("TWO")).await;
    let saved = observe(
        &runtime,
        &mock,
        &inspect(861, "faq", Some("preview"), None),
        Some("TWO"),
    )
    .await;
    let after = definitions(db.pool()).await;
    let audit_count: i64 = sqlx::query_scalar("SELECT count(*) FROM automation_audit_log")
        .fetch_one(db.pool())
        .await
        .expect("all audit kinds");
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
    assert_eq!(
        after, before,
        "including enabled flag, provenance and timestamps"
    );
    assert_eq!(audit_count, 0, "no preview run/mutation/claim fact");
    let expected = "<@3333>\\|tester\\|TWO\\|<#4444>\\|{<@3333>}";
    assert_eq!(draft_body(&draft), expected);
    assert_eq!(
        view_body(&private_content(&saved), "Preview /faq"),
        expected
    );
}

#[tokio::test]
async fn draft_db_does_not_spend_actor_cooldown_or_claim_before_first_real_prefix() {
    let db = database().await;
    seed(db.pool(), "2222", "faq", "Hello {username}", true).await;
    let mock = discord().await;
    let runtime = runtime(db.pool().clone(), &mock, true);
    let before = definitions(db.pool()).await;
    let first = observe(
        &runtime,
        &mock,
        &proposed(870, "!faq {username}"),
        Some("TWO"),
    )
    .await;
    let second = observe(
        &runtime.clone(),
        &mock,
        &proposed(871, "!faq {username}"),
        Some("TWO"),
    )
    .await;
    let preview_facts = execution_facts(db.pool()).await;
    let after = definitions(db.pool()).await;
    let start = mock.requests().len();
    let delivered = runtime
        .handle_message(
            &prefix(872),
            AutomationMessageAcceptance::Unmatched,
            true,
            Some("TWO"),
        )
        .await;
    let replay = runtime
        .handle_message(
            &prefix(872),
            AutomationMessageAcceptance::Unmatched,
            true,
            Some("TWO"),
        )
        .await;
    let next = runtime
        .handle_message(
            &prefix(873),
            AutomationMessageAcceptance::Unmatched,
            true,
            Some("TWO"),
        )
        .await;
    let requests = mock.requests()[start..].to_vec();
    let facts = execution_facts(db.pool()).await;
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
    assert_eq!(after, before);
    assert!(preview_facts.is_empty());
    assert_eq!(draft_body(&first), "!faq tester");
    assert_eq!(draft_body(&second), "!faq tester");
    assert!(
        matches!(delivered, Ok(TextCommandOutcome::Delivered)),
        "{delivered:?}"
    );
    assert!(
        matches!(replay, Ok(TextCommandOutcome::AlreadyAttempted)),
        "{replay:?}"
    );
    assert!(
        matches!(next, Ok(TextCommandOutcome::CoolingDown)),
        "{next:?}"
    );
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with("/channels/4444/messages"));
    let body: Value = serde_json::from_slice(&requests[0].body).expect("prefix reply JSON");
    assert_eq!(body["content"], "Hello tester");
    no_mentions(&body);
    assert_eq!(facts.len(), 2, "only the real prefix's attempt and run");
}
