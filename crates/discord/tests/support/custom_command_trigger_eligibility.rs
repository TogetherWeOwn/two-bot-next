//! Protected behavior: absent dispatch facts must not become an enabled guess.
//! Known effective flags are exercised through the bot gateway acceptance tests.

use super::*;

fn eligibility_line(content: &str) -> String {
    let lower = content.to_lowercase();
    lower
        .lines()
        .find(|line| line.contains("text") && line.contains("eligib"))
        .unwrap_or_else(|| {
            panic!("a separate current text-trigger eligibility line is required: {content}")
        })
        .to_owned()
}

#[tokio::test]
async fn inspect_without_current_dispatch_facts_says_unknown_not_enabled() {
    let db = database().await;
    seed(
        db.pool(),
        "2222",
        "faq",
        "SAVED BODY MUST STAY SEPARATE",
        true,
    )
    .await;
    let before = definitions(db.pool()).await;
    let audit_before: Vec<Value> =
        sqlx::query_scalar("SELECT to_jsonb(a) FROM automation_audit_log a ORDER BY id")
            .fetch_all(db.pool())
            .await
            .expect("snapshot every audit row");
    let mock = discord().await;
    // This legacy constructor receives neither the gateway's current text flag
    // nor a promise that an enabled saved row is dispatchable.
    let runtime = runtime(db.pool().clone(), &mock, true);
    let observed = observe(
        &runtime,
        &mock,
        &inspect(600, "faq", None, None),
        Some("TWO"),
    )
    .await;
    let after = definitions(db.pool()).await;
    let audit_after: Vec<Value> =
        sqlx::query_scalar("SELECT to_jsonb(a) FROM automation_audit_log a ORDER BY id")
            .fetch_all(db.pool())
            .await
            .expect("snapshot every audit row after inspect");
    let unpublished = runtime.published_commands().is_none();
    mock.shutdown().await;
    db.close()
        .await
        .expect("fixture teardown before assertions");
    assert_eq!(after, before, "inspection cannot change saved definitions");
    assert_eq!(
        audit_after, audit_before,
        "inspection cannot append any audit"
    );
    assert!(unpublished, "inspection cannot publish a registry");
    let content = private_content(&observed);
    assert!(content.starts_with("Summary /faq\n"));
    assert!(
        content.to_lowercase().lines().any(|line| {
            line.contains("saved") && line.contains("enabled") && !line.contains("eligib")
        }),
        "saved state needs its own label: {content}"
    );
    let eligibility = eligibility_line(&content);
    assert!(
        eligibility.contains("unknown") || eligibility.contains("unavailable"),
        "missing authoritative facts are not a passing configuration: {content}"
    );
    assert!(
        !eligibility
            .split_once(':')
            .is_some_and(|(_, state)| state.trim_start().starts_with("eligible")),
        "unknown dispatch facts must not be presented as a passing state: {content}"
    );
    assert!(!content.contains("SAVED BODY MUST STAY SEPARATE"));
    assert!(content.contains("/command-inspect name:faq view:template"));
    assert!(content.contains("/command-inspect name:faq view:preview"));
    for claim in [
        "execution: guaranteed",
        "delivery: guaranteed",
        "registration: synchronized",
        "registration: synced",
    ] {
        assert!(
            !content.to_lowercase().contains(claim),
            "unsupported runtime promise: {content}"
        );
    }
}

#[test]
fn inspect_eligibility_compiled_reference_description_exposes_the_distinction() {
    let registry = InteractionRouter::new(gates(true))
        .publish_set(&[])
        .expect("compiled reference input");
    let inspect = registry
        .iter()
        .find(|command| command.name == MANAGEMENT)
        .expect("existing inspect command");
    let description = inspect.description.to_lowercase();
    assert!(
        description.contains("saved") && description.contains("eligib"),
        "generated references must describe saved state separately from eligibility: {description}"
    );
    assert!(inspect.description.encode_utf16().count() <= 100);
}

fn oversized_hostile_row() -> StoredCommand {
    StoredCommand {
        guild_id: "2222".to_owned(),
        name: "a".repeat(32),
        description: "😀**[click](https://example.invalid)**".repeat(150),
        template: "PRIVATE TEMPLATE MUST NOT APPEAR".to_owned(),
        // An unvalidated historical row at the pure projection boundary. The
        // current database regex disallows this; do not weaken it to seed one.
        text_trigger: Some("!**@everyone** [click](https://example.invalid) `code`".to_owned()),
        enabled: true,
    }
}

#[test]
fn inspect_eligibility_summary_budget_and_escaping_control() {
    let row = oversized_hostile_row();
    let content = two_bot_core::custom_commands::format_inspect_summary(&row);
    assert!(content.encode_utf16().count() <= 2000);
    assert!(
        content.contains("truncated"),
        "oversize description is not silently clipped"
    );
    assert!(!content.contains("[click](https://example.invalid)"));
    assert!(!content.contains("**@everyone**"));
    assert!(content.contains("\\*\\*@everyone\\*\\*"));
    assert!(!content.contains(&row.template));
    assert!(content.contains(&format!("/command-inspect name:{} view:template", row.name)));
    assert!(content.contains(&format!("/command-inspect name:{} view:preview", row.name)));
}

#[test]
fn inspect_eligibility_summary_retains_dispatch_facts_when_description_is_truncated() {
    let mut row = oversized_hostile_row();
    row.text_trigger = Some("!faq".to_owned());
    let content = two_bot_core::custom_commands::format_inspect_summary(&row);
    assert!(content.contains("truncated"));
    let eligibility = eligibility_line(&content);
    assert!(
        eligibility.contains("unknown") || eligibility.contains("unavailable"),
        "reserve budget for truthful runtime facts, not just the old description: {content}"
    );
    assert!(content
        .to_lowercase()
        .lines()
        .any(|line| line.contains("saved") && line.contains("enabled")));
}

#[tokio::test]
async fn inspect_eligibility_summary_scope_and_outage_controls_never_guess_a_result() {
    let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new().host("agent-testdb"));
    pool.close().await;
    let mock = discord().await;
    let runtime = runtime(pool, &mock, true);
    for guild in [None, Some(Id::new(7777))] {
        let mut input = inspect(620, "faq", Some("summary"), None);
        input.guild_id = guild;
        assert!(matches!(
            runtime.handle_interaction(&input, Some("TWO")).await,
            Ok(false)
        ));
    }
    assert!(
        mock.requests().is_empty(),
        "no foreign or DM inspection output"
    );
    let observed = observe(
        &runtime,
        &mock,
        &inspect(621, "faq", Some("summary"), None),
        Some("TWO"),
    )
    .await;
    assert!(runtime.published_commands().is_none());
    mock.shutdown().await;
    let content = private_content(&observed);
    assert!(content.contains("temporarily unavailable") && content.contains("try again"));
    assert!(!content.starts_with("Summary /"));
    assert!(!content.contains("PoolClosed") && !content.contains("agent-testdb"));
}
