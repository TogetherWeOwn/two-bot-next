//! TA-02 acceptance battery: the template-assistant endpoint credential is
//! bound through `Secret<T>` and missing bindings fail closed
//! (`docs/command-wiring-security.md` TA-02).
//!
//! Builder contract: this file is published under a manifest and must pass
//! unmodified. Corrections route back to the test author; never edit this
//! file to make an implementation pass.
//!
//! The approved runtime secret path for the endpoint credential is the
//! process-environment key `TWO_ASSISTANT_API_KEY` (an OpenAI-compatible
//! bearer credential, named like the other `TWO_*` secrets). It is a
//! Container secret: declared `EnvOnly` in the settings catalogue, listed in
//! `NOT_FORWARDED` (never in the wrangler flag allowlist), and read only by
//! the assistant gate. `AssistantConfig::from_map` is documented as the
//! whole gate read (`docs/voice-assistant-core.md`), so "no binding" must
//! mean "gate off" there: no command published, no call enabled.
//!
//! Every credential value below is synthetic (`fixture-…`). No real
//! credential, network, database or Discord identity is used. Assertion
//! messages are static so a failure never prints a sentinel.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[path = "support/tracing_capture.rs"]
mod tracing_capture;

use serde_json::json;
use two_bot_core::settings::{
    assert_storable_key, classify_key, is_declared_env_only, is_storable_key, SettingClass,
};
use two_bot_core::voice_assistant::{assistant_command_set, assistant_commands, AssistantConfig};
use two_bot_core::voice_assistant_build::{build_template, AssistantTransport, BuildError};
use two_bot_core::voice_assistant_request::{parse_reply, MAX_REPLY_BYTES};
use two_bot_core::voice_naming::{parse, Evaluation, ExtensionPolicy, Template};
use two_bot_core::voice_rooms::VoiceGates;
use two_bot_core::Secret;

/// The one approved credential binding key. Any other name is not a binding.
const CREDENTIAL_KEY: &str = "TWO_ASSISTANT_API_KEY";
const ENDPOINT_KEY: &str = "TWO_ASSISTANT_ENDPOINT";
const MODEL_KEY: &str = "TWO_ASSISTANT_MODEL";

const CLEAN_ENDPOINT: &str = "https://example.invalid/v1/chat/completions";

/// Synthetic credential values, one per channel so a leak names its source.
const CREDENTIAL: &str = "fixture-assistant-credential-9c41";
const URL_USER: &str = "fixture-assistant-url-user-9c41";
const URL_PASS: &str = "fixture-assistant-url-pass-9c41";
const URL_QUERY: &str = "fixture-assistant-url-query-9c41";
const URL_PATH: &str = "fixture-assistant-url-path-9c41";
const REMOTE_ECHO: &str = "fixture-assistant-remote-echo-9c41";

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

/// Gate map with a valid endpoint, model and bound synthetic credential.
fn bound_vars() -> HashMap<String, String> {
    vars(&[
        (ENDPOINT_KEY, CLEAN_ENDPOINT),
        (MODEL_KEY, "example-model"),
        (CREDENTIAL_KEY, CREDENTIAL),
    ])
}

fn bound_config() -> AssistantConfig {
    AssistantConfig::from_map(&bound_vars()).expect("bound credential enables the gate")
}

fn assert_absent(rendered: &str, sentinels: &[&str], surface: &str) {
    for sentinel in sentinels {
        assert!(
            !rendered.contains(sentinel),
            "synthetic credential reached {surface}"
        );
    }
}

/// Every formatting surface of one value: normal/pretty Debug, plus the
/// nested shapes (`Option`, `Vec`, tuple) diagnostics actually compose.
fn assert_debug_clean(value: &AssistantConfig, sentinels: &[&str]) {
    // Derived Debug is intentionally ignored during dead code analysis,
    // so both fields are read below instead of waiving the lint.
    #[derive(Debug)]
    struct Nested {
        config: AssistantConfig,
        again: Vec<AssistantConfig>,
    }

    let nested = Nested {
        config: value.clone(),
        again: vec![value.clone()],
    };
    assert_eq!(nested.again.len(), 1);
    assert!(!format!("{:?}", nested.config).is_empty());

    let renderings = [
        format!("{value:?}"),
        format!("{value:#?}"),
        format!("{:?}", Some(value.clone())),
        format!("{:?}", vec![value.clone(), value.clone()]),
        format!("{:?}", (value.clone(), "diagnostic context")),
        format!("{nested:?}"),
        format!("{nested:#?}"),
    ];
    for rendered in &renderings {
        assert_absent(rendered, sentinels, "Debug");
    }
}

fn assert_error_clean(error: &(dyn std::error::Error + 'static), sentinels: &[&str]) {
    let mut current = Some(error);
    while let Some(error) = current {
        assert_absent(&error.to_string(), sentinels, "error Display");
        assert_absent(&format!("{error:?}"), sentinels, "error Debug");
        current = error.source();
    }
}

// --- Missing binding fails closed ------------------------------------------

#[test]
fn missing_credential_binding_fails_closed_before_any_call_is_enabled() {
    // Endpoint and model alone must not enable anything: no config, and
    // therefore no published command even with the voice gate on.
    let unbound = vars(&[(ENDPOINT_KEY, CLEAN_ENDPOINT), (MODEL_KEY, "example-model")]);
    let config = AssistantConfig::from_map(&unbound);
    assert_eq!(
        config, None,
        "endpoint without a credential binding must stay disabled"
    );
    let voice = VoiceGates { enabled: true };
    assert!(
        assistant_command_set(&voice, config.as_ref()).is_empty(),
        "unbound assistant must publish no command"
    );
}

#[test]
fn credential_smuggled_outside_the_approved_key_does_not_enable() {
    // A credential value in any other slot is not the approved binding.
    for smuggled in [
        vars(&[(ENDPOINT_KEY, CLEAN_ENDPOINT), (MODEL_KEY, CREDENTIAL)]),
        vars(&[
            (ENDPOINT_KEY, CLEAN_ENDPOINT),
            (MODEL_KEY, "example-model"),
            ("TWO_ASSISTANT_TOKEN", CREDENTIAL),
        ]),
        vars(&[
            (ENDPOINT_KEY, CLEAN_ENDPOINT),
            (MODEL_KEY, "example-model"),
            ("TWO_ASSISTANT_SECRET", CREDENTIAL),
        ]),
        vars(&[
            (ENDPOINT_KEY, CLEAN_ENDPOINT),
            (MODEL_KEY, "example-model"),
            ("ASSISTANT_API_KEY", CREDENTIAL),
        ]),
    ] {
        assert_eq!(
            AssistantConfig::from_map(&smuggled),
            None,
            "only the approved key may bind the credential"
        );
    }
}

#[test]
fn blank_and_control_character_credentials_fail_closed() {
    for credential in [
        "",
        "   ",
        "\t \n",
        "fixture-half\nfixture-half",
        "fixture-half\rfixture-half",
        "fixture-half\0fixture-half",
    ] {
        let map = vars(&[
            (ENDPOINT_KEY, CLEAN_ENDPOINT),
            (MODEL_KEY, "example-model"),
            (CREDENTIAL_KEY, credential),
        ]);
        assert_eq!(
            AssistantConfig::from_map(&map),
            None,
            "blank or control-bearing credential must stay disabled"
        );
    }
}

#[test]
fn credential_without_endpoint_still_fails_closed() {
    let map = vars(&[(CREDENTIAL_KEY, CREDENTIAL), (MODEL_KEY, "example-model")]);
    assert_eq!(AssistantConfig::from_map(&map), None);
}

#[test]
fn bound_credential_enables_config_without_changing_endpoint_or_model() {
    let config = bound_config();
    assert_eq!(config.endpoint(), CLEAN_ENDPOINT);
    assert_eq!(config.model(), "example-model");
    assert_absent(config.endpoint(), &[CREDENTIAL], "endpoint accessor");
    assert_absent(config.model(), &[CREDENTIAL], "model accessor");
}

// --- Sentinel-leak battery ---------------------------------------------------

#[test]
fn bound_credential_never_appears_in_debug_or_nested_debug() {
    assert_debug_clean(&bound_config(), &[CREDENTIAL]);
}

#[test]
fn credential_bearing_endpoint_never_appears_in_debug() {
    // TA-02 wraps credential-bearing endpoints themselves in `Secret<T>`: a
    // sentinel carried in the endpoint path must not render. Refusing such
    // an endpoint outright also satisfies this test.
    let endpoint = format!("https://example.invalid/{URL_PATH}/v1/chat/completions");
    let map = vars(&[
        (ENDPOINT_KEY, &endpoint),
        (MODEL_KEY, "example-model"),
        (CREDENTIAL_KEY, CREDENTIAL),
    ]);
    if let Some(config) = AssistantConfig::from_map(&map) {
        assert_debug_clean(&config, &[CREDENTIAL, URL_PATH]);
    }
}

#[test]
fn secret_display_and_debug_are_constant_for_the_credential() {
    let secret = Secret::new(CREDENTIAL.to_string());
    for rendered in [
        format!("{secret}"),
        format!("{secret:?}"),
        format!("{secret:#?}"),
    ] {
        assert_eq!(
            rendered, "[REDACTED]",
            "Secret formatting must be the constant redaction"
        );
    }
    for rendered in [
        format!("{:?}", Some(secret.clone())),
        format!("{:?}", vec![secret.clone()]),
        format!("{:#?}", Some(secret.clone())),
    ] {
        assert_absent(&rendered, &[CREDENTIAL], "Secret formatting");
    }
    assert_eq!(
        secret.expose(),
        CREDENTIAL,
        "expose is the boundary accessor"
    );
}

#[test]
fn remote_responses_echoing_credentials_never_reach_reply_errors() {
    // Diagnostics for hostile remote bodies: the endpoint echoes credential
    // material back; every error rendering stays sentinel-free.
    let mut oversize = vec![b'x'; MAX_REPLY_BYTES + 1];
    oversize[32..32 + REMOTE_ECHO.len()].copy_from_slice(REMOTE_ECHO.as_bytes());
    let bodies: Vec<Vec<u8>> = vec![
        REMOTE_ECHO.as_bytes().to_vec(),
        json!({"error": {"message": REMOTE_ECHO, "code": CREDENTIAL}})
            .to_string()
            .into_bytes(),
        json!({"choices": [{"message": {"content": REMOTE_ECHO}}]})
            .to_string()
            .into_bytes(),
        format!("{{\"{REMOTE_ECHO}\":\"choices\"}}").into_bytes(),
        oversize,
    ];
    for body in &bodies {
        let error = parse_reply(body).expect_err("hostile body is refused");
        assert_error_clean(&error, &[CREDENTIAL, REMOTE_ECHO]);
    }
}

// --- Scripted transport (no network) -----------------------------------------

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScriptError;

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("scripted transport failure")
    }
}

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

fn one_good() -> Vec<u8> {
    let content = json!({
        "suggestions": [{ "template": "@@owner@@'s room ##", "explanation": "why" }]
    })
    .to_string();
    json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }]
    })
    .to_string()
    .into_bytes()
}

async fn build(
    config: &AssistantConfig,
    transport: &ScriptTransport,
) -> Result<two_bot_core::voice_assistant_build::BuiltTemplate, BuildError<ScriptError>> {
    build_template(
        config,
        "cozy rooms",
        ["@@game_name@@ ##"],
        "General",
        "en-US",
        transport,
        &TestConditions,
    )
    .await
}

#[tokio::test]
async fn build_pipeline_never_receives_credential_in_endpoint_or_body() {
    // The pipeline never sees the credential (authorization is the future
    // transport boundary's business): the endpoint argument and the
    // serialized request body stay sentinel-free, as does the result the
    // admin is shown.
    let config = bound_config();
    let transport = ScriptTransport::new(vec![Ok(one_good())]);
    let built = build(&config, &transport)
        .await
        .expect("valid reply builds");
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    for endpoint in transport.endpoints.lock().unwrap().iter() {
        assert_eq!(endpoint, CLEAN_ENDPOINT);
        assert_absent(endpoint, &[CREDENTIAL], "transport endpoint");
    }
    for body in transport.bodies.lock().unwrap().iter() {
        assert_absent(body, &[CREDENTIAL], "request body");
    }
    assert_absent(&format!("{built:?}"), &[CREDENTIAL], "built reply Debug");
    assert_absent(&built.template, &[CREDENTIAL], "built reply template");
    assert_absent(&built.explanation, &[CREDENTIAL], "built reply explanation");
}

#[tokio::test]
async fn build_errors_never_echo_remote_or_credential_material() {
    let config = bound_config();

    let echoing = json!({"error": {"message": REMOTE_ECHO, "code": CREDENTIAL}})
        .to_string()
        .into_bytes();
    let transport = ScriptTransport::new(vec![Ok(echoing)]);
    let error = build(&config, &transport)
        .await
        .expect_err("error envelope fails the build");
    assert!(matches!(error, BuildError::BadReply(_)));
    assert_absent(
        &error.to_string(),
        &[CREDENTIAL, REMOTE_ECHO],
        "build error Display",
    );
    assert_absent(
        &format!("{error:?}"),
        &[CREDENTIAL, REMOTE_ECHO],
        "build error Debug",
    );

    let transport = ScriptTransport::new(vec![Err(ScriptError)]);
    let error = build(&config, &transport)
        .await
        .expect_err("transport failure fails the build");
    assert_eq!(error.to_string(), "assistant endpoint call failed");
    assert_absent(
        &format!("{error:?}"),
        &[CREDENTIAL],
        "transport error Debug",
    );
}

#[tokio::test]
async fn gate_and_refusal_paths_never_log_credential_material() {
    let capture = tracing_capture::global();
    let offset = capture.text().len();

    let config = bound_config();
    let rejected_endpoint = format!("https://{URL_USER}:{URL_PASS}@example.invalid/v1");
    let rejected = vars(&[
        (ENDPOINT_KEY, &rejected_endpoint),
        (MODEL_KEY, "example-model"),
        (CREDENTIAL_KEY, CREDENTIAL),
    ]);
    let _ = AssistantConfig::from_map(&rejected);
    let transport = ScriptTransport::new(vec![Ok(REMOTE_ECHO.as_bytes().to_vec())]);
    let _ = build(&config, &transport).await;
    tracing::warn!("assistant credential log capture remains active");

    let text = capture.text();
    let slice = &text[offset..];
    assert!(
        slice.contains("assistant credential log capture remains active"),
        "log capture must be live for this battery to mean anything"
    );
    assert_absent(
        slice,
        &[
            CREDENTIAL,
            URL_USER,
            URL_PASS,
            URL_QUERY,
            URL_PATH,
            REMOTE_ECHO,
        ],
        "logs",
    );
}

// --- Credential channels that must refuse -------------------------------------

#[test]
fn credential_in_endpoint_url_is_refused() {
    // With the approved binding present, refusal is attributable to the URL:
    // userinfo and credential query parameters are never a credential path.
    let mut endpoints = vec![
        format!("https://{URL_USER}:{URL_PASS}@example.invalid/v1/chat/completions"),
        format!("https://{URL_PASS}@example.invalid/v1/chat/completions"),
    ];
    for param in [
        "api_key",
        "apikey",
        "key",
        "token",
        "access_token",
        "auth",
        "password",
        "secret",
        "API_KEY",
        "Api-Key",
    ] {
        endpoints.push(format!("{CLEAN_ENDPOINT}?{param}={URL_QUERY}"));
        endpoints.push(format!("{CLEAN_ENDPOINT}?model=x&{param}={URL_QUERY}"));
    }
    for endpoint in &endpoints {
        let map = vars(&[
            (ENDPOINT_KEY, endpoint),
            (MODEL_KEY, "example-model"),
            (CREDENTIAL_KEY, CREDENTIAL),
        ]);
        assert_eq!(
            AssistantConfig::from_map(&map),
            None,
            "credential-bearing URL must be refused"
        );
    }
}

#[test]
fn credential_has_no_request_option_to_travel_in() {
    let defs = assistant_commands();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "templateassistant");
    let mut option_names = Vec::new();
    fn collect(options: &[two_bot_core::commands::CommandOption], names: &mut Vec<String>) {
        for option in options {
            names.push(option.name.clone());
            collect(&option.options, names);
        }
    }
    collect(&defs[0].options, &mut option_names);
    assert_eq!(
        option_names,
        ["request"],
        "the only option is the admin's request text"
    );
    let wire = serde_json::to_string(&defs).expect("definitions serialize");
    assert_absent(
        &wire,
        &[CREDENTIAL_KEY, "api_key", "credential"],
        "command wire shape",
    );
}

#[test]
fn credential_key_is_declared_env_only_and_never_dashboard_storable() {
    assert_eq!(classify_key(CREDENTIAL_KEY), Some(SettingClass::EnvOnly));
    assert!(is_declared_env_only(CREDENTIAL_KEY));
    assert!(!is_storable_key(CREDENTIAL_KEY));
    let error = assert_storable_key(CREDENTIAL_KEY).expect_err("dashboard write refused");
    assert_absent(
        &error.to_string(),
        &[CREDENTIAL],
        "settings refusal Display",
    );
    assert_absent(
        &format!("{error:?}"),
        &[CREDENTIAL],
        "settings refusal Debug",
    );
}

#[test]
fn credential_key_never_travels_through_the_flag_allowlist() {
    // The credential is its own Container secret: classified NOT_FORWARDED
    // with a reason, never a forwarded flag. Endpoint and model stay flags.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = std::fs::read_to_string(root.join("wrangler/src/container-env.ts"))
        .expect("container-env source is readable from the repo root");
    let split = source
        .find("export const NOT_FORWARDED")
        .expect("NOT_FORWARDED block exists");
    let (forwarded_part, not_forwarded_part) = source.split_at(split);
    let quoted_key = format!("\"{CREDENTIAL_KEY}\"");
    assert!(
        !forwarded_part.contains(&quoted_key),
        "credential key must not join FORWARDED_FLAGS"
    );
    assert!(
        not_forwarded_part.contains(&quoted_key),
        "credential key must be classified NOT_FORWARDED with a reason"
    );
    assert!(forwarded_part.contains("\"TWO_ASSISTANT_ENDPOINT\""));
    assert!(forwarded_part.contains("\"TWO_ASSISTANT_MODEL\""));
}
