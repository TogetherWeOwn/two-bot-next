//! Protected acceptance: validate saved templates at the slash/prefix boundary,
//! not by changing the deliberately literal pure renderer. Registered in the
//! existing custom_command_runtime CI target; all HTTP stays on loopback.

use super::{message, runtime, slash, AutomationMessageAcceptance, MockRest, ScriptedResponse};
use super::{CustomCommandRuntime, TextCommandOutcome, PREFIX_REFUSED_SERIES_GUARD};
use crate::common::RestRequest;
use serde_json::{json, Value};
use two_bot_core::{
    custom_command_store as store,
    custom_commands::{render_template, validate_template, StoredCommand, TemplateContext},
};
use two_bot_discord::custom_commands::CustomCommandError;
use two_bot_testsupport::TestDatabase;

const ID: u64 = 870;
const MARKER: &str = "saved-private-marker";

#[derive(Clone, Copy, Debug)]
enum Surface {
    Slash,
    Prefix,
}

#[derive(Debug)]
enum Outcome {
    Slash(bool),
    Prefix(TextCommandOutcome),
}

// The single input matrix also drives validator/preview parity. The shrinking
// length cases catch a missing input guard even when rendered output fits.
#[derive(Clone, Copy, Debug)]
enum Case {
    Unknown,
    Args,
    Empty,
    AsciiOver,
    Utf16Over,
    FourVariables,
    LiteralBraces,
    Utf16Ceiling,
}

impl Case {
    fn template(self) -> String {
        match self {
            Self::Unknown => format!("{MARKER} {{user}} {{usre}}"),
            Self::Args => format!("{MARKER} {{args}}"),
            Self::Empty => String::new(),
            Self::AsciiOver => "{username}".repeat(201),
            Self::Utf16Over => format!("{}{{username}}", "😀".repeat(996)),
            Self::FourVariables => "Hi {user} {username} in {server} {channel}".to_owned(),
            Self::LiteralBraces => "Literal {User} {bad_name} {9} {unfinished".to_owned(),
            Self::Utf16Ceiling => "😀".repeat(1000),
        }
    }

    fn valid(self) -> bool {
        matches!(
            self,
            Self::FourVariables | Self::LiteralBraces | Self::Utf16Ceiling
        )
    }

    fn expected(self) -> String {
        match self {
            Self::FourVariables => "Hi <@3333> tester in Test guild <#4444>".to_owned(),
            Self::LiteralBraces => "Literal {User} {bad_name} {9} {unfinished".to_owned(),
            Self::Utf16Ceiling => "😀".repeat(1000),
            _ => panic!("invalid definitions have no public output"),
        }
    }
}

const INVALID: [Case; 5] = [
    Case::Unknown,
    Case::Args,
    Case::Empty,
    Case::AsciiOver,
    Case::Utf16Over,
];
const VALID: [Case; 3] = [Case::FourVariables, Case::LiteralBraces, Case::Utf16Ceiling];

async fn database() -> TestDatabase {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        // The existing legacy target step prepares this bootstrap but does not
        // export TWO_TEST_DATABASE_URL. Select it only under that explicit CI
        // opt-in; TestDatabase still validates it. Never retry a failed URL.
        Err(std::env::VarError::NotPresent)
            if std::env::var("CI").as_deref() == Ok("true")
                && std::env::var("TWO_CUSTOM_COMMAND_TEST_CI").as_deref() == Ok("1") =>
        {
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci".to_owned()
        }
        Err(_) => panic!("disposable test bootstrap required"),
    };
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create isolated migrated test DB")
}

async fn seed(db: &TestDatabase, case: Case, enabled: bool) {
    let template = case.template();
    if template.is_empty() || template.chars().count() > 2000 {
        // Model a corrupt/imported row only inside this owned disposable DB.
        // Real migrations and validated service writers remain untouched.
        sqlx::query("ALTER TABLE automation_commands DROP CONSTRAINT automation_commands_template")
            .execute(db.pool())
            .await
            .expect("fixture-only removal of storage length check");
    }
    store::put_command(
        db.pool(),
        &StoredCommand {
            guild_id: "2222".to_owned(),
            name: "faq".to_owned(),
            description: "Saved response".to_owned(),
            template,
            text_trigger: Some("!faq".to_owned()),
            enabled,
        },
        "3333",
        "3333",
        "2026-10-01T00:00:00Z",
        "2026-10-02T00:00:00Z",
    )
    .await
    .expect("directly seed saved fixture, never weaken the validated writer");
}

async fn definitions(db: &TestDatabase) -> Vec<Value> {
    sqlx::query_scalar("SELECT to_jsonb(c) FROM automation_commands c ORDER BY guild_id, name")
        .fetch_all(db.pool())
        .await
        .expect("snapshot every definition column")
}

async fn facts(db: &TestDatabase) -> Vec<Value> {
    sqlx::query_scalar("SELECT to_jsonb(a) FROM automation_audit_log a ORDER BY id")
        .fetch_all(db.pool())
        .await
        .expect("snapshot complete audit facts")
}

async fn discord() -> MockRest {
    MockRest::with_responder(|request| match request.method.as_str() {
        "POST" if request.path.ends_with("/callback") => ScriptedResponse::status(204),
        "PATCH" if request.path.ends_with("/messages/@original") => {
            ScriptedResponse::json(200, json!({"id": "9000"}))
        }
        "POST" if request.path.ends_with("/channels/4444/messages") => {
            ScriptedResponse::json(200, json!({"id": "9001"}))
        }
        _ => ScriptedResponse::status(403),
    })
    .await
}

async fn invoke(
    runtime: &CustomCommandRuntime,
    surface: Surface,
    server: Option<&str>,
) -> Result<Outcome, CustomCommandError> {
    match surface {
        Surface::Slash => runtime
            .handle_interaction(&slash(ID, "faq", 0, &[]), server)
            .await
            .map(Outcome::Slash),
        Surface::Prefix => runtime
            .handle_message(
                &message(ID, "!FaQ ignored arguments"),
                AutomationMessageAcceptance::Unmatched,
                true,
                server,
            )
            .await
            .map(Outcome::Prefix),
    }
}

struct Observed {
    result: Result<Outcome, CustomCommandError>,
    requests: Vec<RestRequest>,
    facts: Vec<Value>,
    before: Vec<Value>,
    after: Vec<Value>,
}

async fn observe(case: Case, surface: Surface, enabled: bool, server: Option<&str>) -> Observed {
    let db = database().await;
    seed(&db, case, enabled).await;
    let before = definitions(&db).await;
    let mock = discord().await;
    let result = invoke(&runtime(db.pool().clone(), &mock, true), surface, server).await;
    let observed = Observed {
        result,
        requests: mock.requests(),
        facts: facts(&db).await,
        before,
        after: definitions(&db).await,
    };
    mock.shutdown().await;
    db.close()
        .await
        .expect("fixture teardown before behavioral assertions");
    observed
}

fn body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("outgoing JSON")
}

fn no_mentions(value: &Value) {
    assert_eq!(value["allowed_mentions"]["parse"], json!([]));
    for key in ["users", "roles"] {
        assert!(
            value["allowed_mentions"][key].is_null() || value["allowed_mentions"][key] == json!([])
        );
    }
    assert_ne!(value["allowed_mentions"]["replied_user"], true);
}

fn slash_completion(requests: &[RestRequest]) -> Value {
    assert_eq!(
        requests.len(),
        2,
        "one defer, one completion; no channel/registry calls"
    );
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with(&format!(
        "/interactions/{ID}/custom-command-fixture/callback"
    )));
    let defer = body(&requests[0]);
    assert_eq!(
        defer["type"], 5,
        "preserve the existing acknowledgement contract"
    );
    assert!(defer["data"]["content"].is_null() || defer["data"]["content"] == "");
    assert_eq!(requests[1].method, "PATCH");
    assert!(requests[1]
        .path
        .ends_with("/webhooks/1111/custom-command-fixture/messages/@original"));
    let completion = body(&requests[1]);
    no_mentions(&completion);
    completion
}

fn unavailable(requests: &[RestRequest]) {
    let completion = slash_completion(requests);
    let content = completion["content"]
        .as_str()
        .expect("bounded availability guidance");
    assert!(!content.trim().is_empty() && content.encode_utf16().count() <= 300);
    assert!(
        content.to_lowercase().contains("unavailable"),
        "truthful unavailable guidance"
    );
    for forbidden in [
        MARKER,
        "{usre}",
        "{args}",
        "<@3333>",
        "tester",
        "Test guild",
        "😀",
    ] {
        assert!(
            !content.contains(forbidden),
            "fallback must not echo a template or expansion"
        );
    }
    for key in ["embeds", "attachments", "components"] {
        assert!(
            completion[key].is_null() || completion[key] == json!([]),
            "content-only fallback"
        );
    }
}

fn audit(surface: Surface, rows: &[Value], outcome: &str, reason: Option<&str>) {
    let id = match surface {
        Surface::Slash => format!("custom:run:{ID}"),
        Surface::Prefix => format!("custom:text:result:{ID}"),
    };
    assert_eq!(
        rows.len(),
        if matches!(surface, Surface::Slash) {
            1
        } else {
            2
        }
    );
    let row = rows
        .iter()
        .find(|row| row["id"] == id)
        .expect("one definitive run fact");
    assert_eq!(row["guild_id"], "2222");
    assert_eq!(row["actor_id"], "3333");
    assert_eq!(row["action"], "command.run");
    assert_eq!(row["target_key"], "faq");
    assert_eq!(row["outcome"], outcome);
    assert_eq!(
        row["reason"],
        reason.map_or(Value::Null, |reason| json!(reason))
    );
    if matches!(surface, Surface::Prefix) {
        let claim = rows
            .iter()
            .find(|row| row["action"] == "command.text_attempt")
            .expect("durable reservation");
        assert_eq!(claim["id"], format!("custom:text:attempt:{ID}"));
        assert_eq!(claim["outcome"], "unknown");
        assert_eq!(claim["reason"], "delivery_pending");
    }
    let serialized = serde_json::to_string(rows).expect("audit JSON");
    for forbidden in [
        MARKER,
        "{usre}",
        "{args}",
        "<@",
        "tester",
        "Test guild",
        "😀",
    ] {
        assert!(!serialized.contains(forbidden), "audit is content-free");
    }
}

fn refusal_outcome(surface: Surface, result: &Result<Outcome, CustomCommandError>) {
    // Successful fallback completion can report ownership rather than an error.
    // The wire body and failed run fact, not a new return API, prove refusal.
    let refused = match surface {
        Surface::Slash => matches!(
            result,
            Ok(Outcome::Slash(true)) | Err(CustomCommandError::Render)
        ),
        Surface::Prefix => matches!(result, Err(CustomCommandError::Render)),
    };
    assert!(
        refused,
        "validation refusal must not become a storage/wire error: {result:?}"
    );
}

fn private_preview(requests: &[RestRequest]) -> Value {
    assert!(!requests.is_empty());
    let callback = body(&requests[0]);
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with(&format!(
        "/interactions/{ID}/custom-command-fixture/callback"
    )));
    assert_eq!(callback["data"]["flags"].as_u64().unwrap_or(0) & 64, 64);
    let content = if callback["type"] == 5 {
        slash_completion(requests)
    } else {
        assert_eq!(callback["type"], 4);
        assert_eq!(
            requests.len(),
            1,
            "one immediate private response, no execution"
        );
        callback["data"].clone()
    };
    no_mentions(&content);
    let text = content["content"]
        .as_str()
        .expect("private correction content");
    assert!(!text.is_empty() && text.encode_utf16().count() <= 2000);
    content
}

async fn refuses(case: Case, surface: Surface) {
    let observed = observe(case, surface, true, Some("Test guild")).await;
    assert_eq!(
        observed.after, observed.before,
        "never disable/delete/repair a corrupt definition"
    );
    match surface {
        Surface::Slash => unavailable(&observed.requests),
        Surface::Prefix => assert!(
            observed.requests.is_empty(),
            "invalid stored template must never POST"
        ),
    }
    refusal_outcome(surface, &observed.result);
    audit(surface, &observed.facts, "failed", Some("render_failed"));
}

#[tokio::test]
async fn slash_refuses_unknown_saved_placeholder() {
    refuses(Case::Unknown, Surface::Slash).await;
}
#[tokio::test]
async fn prefix_refuses_unknown_saved_placeholder() {
    refuses(Case::Unknown, Surface::Prefix).await;
}
#[tokio::test]
async fn slash_does_not_expand_args_language() {
    refuses(Case::Args, Surface::Slash).await;
}
#[tokio::test]
async fn prefix_does_not_expand_args_language() {
    refuses(Case::Args, Surface::Prefix).await;
}
#[tokio::test]
async fn slash_refuses_zero_length_saved_template() {
    refuses(Case::Empty, Surface::Slash).await;
}
#[tokio::test]
async fn prefix_refuses_zero_length_saved_template() {
    refuses(Case::Empty, Surface::Prefix).await;
}
#[tokio::test]
async fn slash_validates_input_length_even_when_ascii_expansion_shrinks() {
    refuses(Case::AsciiOver, Surface::Slash).await;
}
#[tokio::test]
async fn prefix_validates_input_length_even_when_ascii_expansion_shrinks() {
    refuses(Case::AsciiOver, Surface::Prefix).await;
}
#[tokio::test]
async fn slash_validates_utf16_input_length_even_when_expansion_shrinks() {
    refuses(Case::Utf16Over, Surface::Slash).await;
}
#[tokio::test]
async fn prefix_validates_utf16_input_length_even_when_expansion_shrinks() {
    refuses(Case::Utf16Over, Surface::Prefix).await;
}

#[test]
fn matrix_preserves_validator_and_literal_renderer_contracts() {
    let context = TemplateContext {
        user: "<@3333>".to_owned(),
        username: "tester".to_owned(),
        server: "Test guild".to_owned(),
        channel: "<#4444>".to_owned(),
    };
    for case in INVALID.into_iter().chain(VALID) {
        assert_eq!(
            validate_template(&case.template()).is_ok(),
            case.valid(),
            "{case:?}"
        );
    }
    assert_eq!(
        render_template(&Case::Unknown.template(), &context).expect("pure renderer stays literal"),
        "saved-private-marker <@3333> {usre}"
    );
    assert_eq!(
        render_template(&Case::Args.template(), &context).expect("args stays literal"),
        "saved-private-marker {args}"
    );
    assert_eq!(
        render_template(&Case::AsciiOver.template(), &context)
            .expect("shrinking ASCII output fits"),
        "tester".repeat(201)
    );
    assert_eq!(
        render_template(&Case::Utf16Over.template(), &context)
            .expect("shrinking UTF-16 output fits"),
        format!("{}tester", "😀".repeat(996))
    );
}

#[tokio::test]
async fn valid_four_variables_literal_braces_and_utf16_ceiling_stay_byte_identical() {
    for case in VALID {
        for surface in [Surface::Slash, Surface::Prefix] {
            let observed = observe(case, surface, true, Some("Test guild")).await;
            assert_eq!(observed.after, observed.before);
            let payload = match surface {
                Surface::Slash => {
                    assert!(matches!(observed.result, Ok(Outcome::Slash(true))));
                    slash_completion(&observed.requests)
                }
                Surface::Prefix => {
                    assert!(matches!(
                        observed.result,
                        Ok(Outcome::Prefix(TextCommandOutcome::Delivered))
                    ));
                    assert_eq!(observed.requests.len(), 1);
                    assert_eq!(observed.requests[0].method, "POST");
                    assert!(observed.requests[0]
                        .path
                        .ends_with("/channels/4444/messages"));
                    let payload = body(&observed.requests[0]);
                    assert_eq!(payload["enforce_nonce"], true);
                    assert_eq!(payload["nonce"], ID);
                    no_mentions(&payload);
                    payload
                }
            };
            assert_eq!(payload["content"], case.expected(), "{case:?} {surface:?}");
            audit(surface, &observed.facts, "ok", None);
        }
    }
}

#[tokio::test]
async fn disabled_corrupt_rows_stay_silent_and_absent_context_stays_a_local_failure() {
    for surface in [Surface::Slash, Surface::Prefix] {
        let disabled = observe(Case::Unknown, surface, false, Some("Test guild")).await;
        assert_eq!(disabled.after, disabled.before);
        assert!(disabled.requests.is_empty() && disabled.facts.is_empty());
        assert!(matches!(
            disabled.result,
            Ok(Outcome::Slash(false)) | Ok(Outcome::Prefix(TextCommandOutcome::Ignored))
        ));
        let missing_context = observe(Case::FourVariables, surface, true, None).await;
        assert_eq!(missing_context.after, missing_context.before);
        let reason = match surface {
            Surface::Slash => {
                assert!(matches!(
                    missing_context.result,
                    Ok(Outcome::Slash(true)) | Err(CustomCommandError::Context)
                ));
                unavailable(&missing_context.requests);
                "render_failed"
            }
            Surface::Prefix => {
                assert!(matches!(
                    missing_context.result,
                    Err(CustomCommandError::Context)
                ));
                assert!(missing_context.requests.is_empty());
                "context_unavailable"
            }
        };
        audit(surface, &missing_context.facts, "failed", Some(reason));
    }
}

#[tokio::test]
async fn invalid_saved_preview_agrees_with_validator_without_claiming_a_run() {
    for case in INVALID {
        let db = database().await;
        seed(&db, case, true).await;
        let before = definitions(&db).await;
        let mock = discord().await;
        let result = runtime(db.pool().clone(), &mock, true)
            .handle_interaction(
                &slash(
                    ID,
                    "command-inspect",
                    32,
                    &[("name", "faq"), ("view", "preview")],
                ),
                Some("Test guild"),
            )
            .await;
        let requests = mock.requests();
        let after = definitions(&db).await;
        let rows = facts(&db).await;
        mock.shutdown().await;
        db.close().await.expect("fixture teardown");
        assert!(matches!(result, Ok(true)), "private inspect is owned");
        assert_eq!(after, before);
        assert!(
            rows.is_empty(),
            "preview never claims/runs/audits execution"
        );
        let completion = private_preview(&requests);
        let content = completion["content"].as_str().expect("private correction");
        assert!(content.contains("/command name:faq template:"));
        assert!(!content.contains(MARKER) && !content.contains("Preview /faq"));
    }
}

#[tokio::test]
async fn invalid_prefix_replay_is_reserved_once_across_runtime_restart() {
    let db = database().await;
    seed(&db, Case::Unknown, true).await;
    let before = definitions(&db).await;
    let mock = discord().await;
    let first = invoke(
        &runtime(db.pool().clone(), &mock, true),
        Surface::Prefix,
        Some("Test guild"),
    )
    .await;
    let original = facts(&db).await;
    let replay = invoke(
        &runtime(db.pool().clone(), &mock, true),
        Surface::Prefix,
        Some("Test guild"),
    )
    .await;
    let after_facts = facts(&db).await;
    let after = definitions(&db).await;
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
    assert!(
        requests.is_empty(),
        "neither invalid first run nor replay sends a body"
    );
    assert!(matches!(first, Err(CustomCommandError::Render)));
    assert!(matches!(
        replay,
        Ok(Outcome::Prefix(TextCommandOutcome::AlreadyAttempted))
    ));
    assert_eq!(
        after_facts, original,
        "preserve every audit column including created_at"
    );
    assert_eq!(after, before);
    audit(Surface::Prefix, &original, "failed", Some("render_failed"));
}

#[tokio::test]
async fn invalid_slash_replay_keeps_first_render_failure_immutable() {
    let db = database().await;
    seed(&db, Case::Unknown, true).await;
    let before = definitions(&db).await;
    let mock = discord().await;
    let first = invoke(
        &runtime(db.pool().clone(), &mock, true),
        Surface::Slash,
        Some("Test guild"),
    )
    .await;
    let original = facts(&db).await;
    let replay = invoke(
        &runtime(db.pool().clone(), &mock, true),
        Surface::Slash,
        Some("Test guild"),
    )
    .await;
    let rows = facts(&db).await;
    let after = definitions(&db).await;
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
    assert_eq!(
        requests.len(),
        4,
        "one defer/completion per explicit entry; no automatic retry"
    );
    unavailable(&requests[..2]);
    unavailable(&requests[2..]);
    refusal_outcome(Surface::Slash, &first);
    refusal_outcome(Surface::Slash, &replay);
    assert_eq!(
        rows, original,
        "first-write-wins, not a Storage error on duplicate"
    );
    assert_eq!(after, before);
    audit(Surface::Slash, &rows, "failed", Some("render_failed"));
}

#[tokio::test]
async fn invalid_row_defer_refusal_and_ambiguity_keep_existing_audit_semantics() {
    for status in [403, 500, 429] {
        let db = database().await;
        seed(&db, Case::Unknown, true).await;
        let before = definitions(&db).await;
        let response = if status == 429 {
            ScriptedResponse::rate_limited(0.01, "0.01")
        } else {
            ScriptedResponse::status(status)
        };
        let mock = MockRest::start(vec![response], ScriptedResponse::status(500)).await;
        let result = invoke(
            &runtime(db.pool().clone(), &mock, true),
            Surface::Slash,
            Some("Test guild"),
        )
        .await;
        let first = facts(&db).await;
        let requests = mock.requests();
        mock.shutdown().await;
        // Explicit re-entry after the wire error must not overwrite a definitive
        // refusal, nor invent a fact for the earlier ambiguous acknowledgement.
        let mock = discord().await;
        let retry = invoke(
            &runtime(db.pool().clone(), &mock, true),
            Surface::Slash,
            Some("Test guild"),
        )
        .await;
        let rows = facts(&db).await;
        let retry_requests = mock.requests();
        let after = definitions(&db).await;
        mock.shutdown().await;
        db.close().await.expect("fixture teardown");
        assert_eq!(requests.len(), 1, "only the defer; no completion or resend");
        assert_eq!(requests[0].method, "POST");
        assert!(requests[0].path.ends_with("/callback"));
        if status == 403 {
            assert!(matches!(result, Err(CustomCommandError::Delivery)));
            audit(Surface::Slash, &first, "failed", Some("delivery_failed"));
            assert_eq!(rows, first);
        } else {
            assert!(matches!(result, Err(CustomCommandError::DeliveryUnknown)));
            assert!(
                first.is_empty(),
                "ambiguous defer has no definitive run fact"
            );
            audit(Surface::Slash, &rows, "failed", Some("render_failed"));
        }
        assert_eq!(after, before);
        unavailable(&retry_requests);
        refusal_outcome(Surface::Slash, &retry);
    }
}

#[tokio::test]
async fn invalid_local_render_fact_survives_refused_or_ambiguous_fallback_completion() {
    for (definitive, response) in [
        (true, ScriptedResponse::status(403)),
        (false, ScriptedResponse::status(500)),
        (false, ScriptedResponse::rate_limited(0.01, "0.01")),
        (
            false,
            ScriptedResponse::json(200, json!({"id": "9000"})).truncated_body(),
        ),
    ] {
        let db = database().await;
        seed(&db, Case::Unknown, true).await;
        let before = definitions(&db).await;
        let mock = MockRest::start(
            vec![ScriptedResponse::status(204), response],
            ScriptedResponse::status(500),
        )
        .await;
        let result = invoke(
            &runtime(db.pool().clone(), &mock, true),
            Surface::Slash,
            Some("Test guild"),
        )
        .await;
        let requests = mock.requests();
        let rows = facts(&db).await;
        let after = definitions(&db).await;
        mock.shutdown().await;
        db.close().await.expect("fixture teardown");
        unavailable(&requests);
        if definitive {
            assert!(matches!(result, Err(CustomCommandError::Delivery)));
        } else {
            assert!(matches!(result, Err(CustomCommandError::DeliveryUnknown)));
        }
        audit(Surface::Slash, &rows, "failed", Some("render_failed"));
        assert_eq!(
            after, before,
            "wire ambiguity never repairs a saved definition"
        );
    }
}

#[tokio::test]
async fn corrupt_rows_do_not_bypass_prefix_acceptance_scope_or_feature_gates() {
    let _guard = PREFIX_REFUSED_SERIES_GUARD.lock().await;
    let db = database().await;
    seed(&db, Case::Unknown, true).await;
    let before = definitions(&db).await;
    let mock = discord().await;
    for acceptance in [
        AutomationMessageAcceptance::Matched,
        AutomationMessageAcceptance::Unavailable,
        AutomationMessageAcceptance::CaptureOnly,
    ] {
        let result = runtime(db.pool().clone(), &mock, true)
            .handle_message(&message(ID, "!faq"), acceptance, true, Some("Test guild"))
            .await;
        assert!(matches!(result, Ok(TextCommandOutcome::Refused)));
    }
    for scope in [
        "text-off",
        "automations-off",
        "bot",
        "webhook",
        "foreign",
        "dm",
        "not-first",
    ] {
        let mut input = message(
            ID,
            if scope == "not-first" {
                "hello !faq"
            } else {
                "!faq"
            },
        );
        match scope {
            "bot" => input.author.bot = true,
            "webhook" => input.webhook_id = Some(twilight_model::id::Id::new(6666)),
            "foreign" => input.guild_id = Some(twilight_model::id::Id::new(7777)),
            "dm" => input.guild_id = None,
            _ => {}
        }
        let result = runtime(db.pool().clone(), &mock, scope != "automations-off")
            .handle_message(
                &input,
                AutomationMessageAcceptance::Unmatched,
                scope != "text-off",
                Some("Test guild"),
            )
            .await;
        assert!(matches!(result, Ok(TextCommandOutcome::Ignored)), "{scope}");
    }
    let after = definitions(&db).await;
    let rows = facts(&db).await;
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("fixture teardown");
    assert_eq!(after, before);
    assert!(rows.is_empty() && requests.is_empty());
}
