//! Hermetic V12 acceptance cases for the select-and-retry build pipeline:
//! a scripted transport stands in for the endpoint, so no network,
//! database, Discord, credentials or staging identity is used.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use serde_json::json;
use two_bot_core::voice_assistant::AssistantConfig;
use two_bot_core::voice_assistant_build::{
    build_template, AssistantTransport, BuildError, MAX_BUILD_ATTEMPTS,
};
use two_bot_core::voice_assistant_request::{ModelNameError, ReplyError, MAX_REPLY_BYTES};
use two_bot_core::voice_assistant_validate::TemplateRefusal;
use two_bot_core::voice_naming::{parse, Evaluation, ExtensionPolicy, Template};

/// Token-safe and lowercase, so `@@{MARKER}@@` parses as an unknown token
/// whose name the parser keeps verbatim.
const MARKER: &str = "leak_marker_7f3a";

/// Conditions policy with known truth values over the six scenarios, mirroring
/// the lint suite's stand-in: `LIVE` fires only when someone streams,
/// `FULL` only when the room hit its limit, `GAME` when anyone plays,
/// `WEEKEND` on Saturday/Sunday, and `PRIVATE` nowhere.
struct TestConditions;

impl ExtensionPolicy for TestConditions {
    fn conditional(&self, source: &str, evaluation: &mut Evaluation<'_, Self>) -> String {
        let inner = &source[2..source.len() - 2];
        let Some((condition, branches)) = inner.split_once("??") else {
            return String::new();
        };
        let (yes, no) = branches.split_once("//").unwrap_or((branches, ""));
        let room = evaluation.context();
        let truth = match condition.trim() {
            "LIVE" => room.live_count > 0,
            "FULL" => room.user_limit > 0 && room.member_count >= room.user_limit,
            "GAME" => room.members_playing > 0,
            "WEEKEND" => matches!((room.timestamp.div_euclid(86_400) + 4) % 7, 0 | 6),
            _ => false,
        };
        evaluation.evaluate(&parse(if truth { yes } else { no }))
    }

    fn styled(
        &self,
        _modes: &str,
        body: &Template,
        _source: &str,
        evaluation: &mut Evaluation<'_, Self>,
    ) -> String {
        evaluation.evaluate(body)
    }
}

const CONDITIONS: TestConditions = TestConditions;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScriptError;

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("scripted transport failure")
    }
}

/// Scripted endpoint: each call records its inputs and pops the next reply.
struct ScriptTransport {
    replies: Mutex<VecDeque<Result<Vec<u8>, ScriptError>>>,
    calls: AtomicUsize,
    endpoints: Mutex<Vec<String>>,
    bodies: Mutex<Vec<String>>,
}

impl ScriptTransport {
    fn new(replies: Vec<Result<Vec<u8>, ScriptError>>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            calls: AtomicUsize::new(0),
            endpoints: Mutex::new(Vec::new()),
            bodies: Mutex::new(Vec::new()),
        }
    }
}

impl AssistantTransport for ScriptTransport {
    type Error = ScriptError;

    async fn post_chat_completions(
        &self,
        endpoint: &str,
        body: &str,
    ) -> Result<Vec<u8>, ScriptError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.endpoints.lock().unwrap().push(endpoint.to_string());
        self.bodies.lock().unwrap().push(body.to_string());
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted")
    }
}

fn config() -> AssistantConfig {
    AssistantConfig::from_map(&HashMap::from([
        (
            "TWO_ASSISTANT_ENDPOINT".to_string(),
            "https://example.com/v1/chat/completions".to_string(),
        ),
        (
            "TWO_ASSISTANT_MODEL".to_string(),
            "example-model".to_string(),
        ),
    ]))
    .expect("endpoint enables")
}

fn envelope(content: &str) -> Vec<u8> {
    json!({
        "id": "reply-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }]
    })
    .to_string()
    .into_bytes()
}

fn suggestions(items: serde_json::Value) -> Vec<u8> {
    envelope(&json!({ "suggestions": items }).to_string())
}

fn one(template: &str) -> Vec<u8> {
    suggestions(json!([{ "template": template, "explanation": "why" }]))
}

async fn build(
    transport: &ScriptTransport,
    request: &str,
) -> Result<two_bot_core::voice_assistant_build::BuiltTemplate, BuildError<ScriptError>> {
    build_template(
        &config(),
        request,
        ["@@game_name@@ ##"],
        "General",
        "en-US",
        transport,
        &CONDITIONS,
    )
    .await
}

// --- happy path -------------------------------------------------------------

#[tokio::test]
async fn first_attempt_accept_returns_template_explanation_and_six_previews() {
    let transport = ScriptTransport::new(vec![Ok(one("@@owner@@'s room ##"))]);
    let built = build(&transport, "cozy rooms").await.expect("valid reply");
    assert_eq!(built.template, "@@owner@@'s room ##");
    assert_eq!(built.explanation, "why");
    assert_eq!(
        built.previews.clone().map(|render| render.name),
        [
            "Avery's room #1",
            "Blake's room #2",
            "Casey's room #3",
            "Devon's room #4",
            "Emery's room #5",
            "Finley's room #6",
        ]
        .map(String::from)
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        transport.endpoints.lock().unwrap().as_slice(),
        ["https://example.com/v1/chat/completions"]
    );
}

#[tokio::test]
async fn best_first_order_picks_the_first_valid_suggestion() {
    let transport = ScriptTransport::new(vec![Ok(suggestions(json!([
        {"template": "@@ownr@@'s room", "explanation": "misspelled"},
        {"template": "@@owner@@'s room ##", "explanation": "fixed"},
    ])))]);
    let built = build(&transport, "cozy rooms").await.expect("second wins");
    assert_eq!(built.template, "@@owner@@'s room ##");
    assert_eq!(built.explanation, "fixed");
    // No regeneration: the valid suggestion was in the same reply.
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
}

// --- regeneration -----------------------------------------------------------

#[tokio::test]
async fn refused_set_is_regenerated_before_the_admin_sees_anything() {
    let transport = ScriptTransport::new(vec![
        Ok(one("@@stream_name@@")),
        Ok(one("@@owner@@'s room ##")),
    ]);
    let built = build(&transport, "cozy rooms")
        .await
        .expect("regenerated reply wins");
    assert_eq!(built.template, "@@owner@@'s room ##");
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    // Every attempt sends the same deterministic body.
    let bodies = transport.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1]);
}

#[tokio::test]
async fn exhausted_attempts_fail_with_the_last_refusal() {
    let transport = ScriptTransport::new(vec![
        Ok(one("@@ownr@@'s room")),
        Ok(one("@@stream_name@@")),
        Ok(one("@@nope@@")),
        Ok(one("@@owner@@'s room ##")),
    ]);
    let Err(BuildError::NoUsableSuggestion { attempts, refusal }) =
        build(&transport, "cozy rooms").await
    else {
        panic!("expected exhaustion");
    };
    assert_eq!(attempts, MAX_BUILD_ATTEMPTS);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 3);
    // The fourth (valid) scripted reply is never consumed.
    assert_eq!(transport.replies.lock().unwrap().len(), 1);
    assert_eq!(refusal, TemplateRefusal::UnknownToken);
}

// --- immediate failures -----------------------------------------------------

#[tokio::test]
async fn malformed_reply_fails_without_retry() {
    for reply in [
        envelope("null"),
        vec![0u8; MAX_REPLY_BYTES + 1],
        suggestions(json!([])),
    ] {
        let transport = ScriptTransport::new(vec![Ok(reply)]);
        let Err(error) = build(&transport, "cozy rooms").await else {
            panic!("expected a bad-reply failure");
        };
        assert!(
            matches!(error, BuildError::BadReply(_)),
            "unexpected error: {error:?}"
        );
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn transport_failure_fails_the_build() {
    let transport = ScriptTransport::new(vec![Err(ScriptError)]);
    let Err(BuildError::Transport(ScriptError)) = build(&transport, "cozy rooms").await else {
        panic!("expected a transport failure");
    };
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn blank_request_never_calls_the_endpoint() {
    let transport = ScriptTransport::new(vec![]);
    let Err(BuildError::EmptyRequest) = build(&transport, "   ").await else {
        panic!("expected an empty-request failure");
    };
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn blank_model_fails_before_any_call() {
    let unmodeled = AssistantConfig::from_map(&HashMap::from([(
        "TWO_ASSISTANT_ENDPOINT".to_string(),
        "https://example.com/v1".to_string(),
    )]))
    .expect("endpoint enables");
    let transport = ScriptTransport::new(vec![]);
    let Err(BuildError::BadModel(ModelNameError::Blank)) = build_template(
        &unmodeled,
        "cozy rooms",
        ["@@game_name@@ ##"],
        "General",
        "en-US",
        &transport,
        &CONDITIONS,
    )
    .await
    else {
        panic!("expected a blank-model failure");
    };
    assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
}

// --- error hygiene ----------------------------------------------------------

#[tokio::test]
async fn build_errors_repeat_no_reply_or_template_text() {
    let bad = format!("@@{MARKER}@@'s room");
    let transport = ScriptTransport::new(vec![Ok(one(&bad)), Ok(one(&bad)), Ok(one(&bad))]);
    let Err(error) = build(&transport, "cozy rooms").await else {
        panic!("expected exhaustion");
    };
    assert!(!error.to_string().contains(MARKER), "{error}");
    assert!(!format!("{error:?}").contains(MARKER), "{error:?}");
    assert!(matches!(
        error,
        BuildError::NoUsableSuggestion {
            refusal: TemplateRefusal::UnknownToken,
            ..
        }
    ));

    // A shape failure still fails the whole reply at once: the marker rides
    // in a position the static error never repeats.
    let transport = ScriptTransport::new(vec![Ok(suggestions(json!([MARKER])))]);
    let Err(error) = build(&transport, "cozy rooms").await else {
        panic!("expected a bad-reply failure");
    };
    assert!(
        matches!(
            error,
            BuildError::BadReply(ReplyError::WrongShape {
                at: "suggestions[].template"
            })
        ),
        "unexpected error: {error:?}"
    );
    assert!(!error.to_string().contains(MARKER), "{error}");
    assert!(!format!("{error:?}").contains(MARKER), "{error:?}");
}
