//! Protected acceptance at the RoomHttp wire boundary; synthetic loopback traffic only.
#![cfg(test)]

use super::super::{RoomHttp, RoomHttpError};
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, Method, Response, StatusCode, Uri},
    Router,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle, time::Instant};
use twilight_model::{
    channel::message::{
        component::{ActionRow, Button, ButtonStyle},
        AllowedMentions, Component, MessageFlags,
    },
    http::{
        attachment::Attachment,
        interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
    },
    id::Id,
};

const SUMMARY: &str = "Proposed voice import: 2 changes. Review the complete JSON before Confirm. <@300> @everyone 🛰️";
const FILENAME: &str = "voice-import-review-v1.json";
const DESCRIPTION: &str = "Complete proposed voice configuration changes";
const TOKEN: &str = "synthetic-interaction-token";
const CALLBACK: &str = "/api/v10/interactions/2/synthetic-interaction-token/callback";
const ORIGINAL: &str = "/api/v10/webhooks/1/synthetic-interaction-token/messages/@original";
const CONFIRM: &str = "two:voice:import-confirm:300:0123456789abcdef";
const CANCEL: &str = "two:voice:import-cancel:300:0123456789abcdef";
const MAX_CAPTURE_BYTES: usize = 262_144;
const MAX_REQUESTS: usize = 8;

struct RawRequest {
    method: Method,
    path: String,
    content_types: Vec<String>,
    authorized: bool,
    body: Bytes,
}

#[derive(Clone)]
struct Capture {
    requests: Arc<Mutex<Vec<RawRequest>>>,
    attempts: Arc<AtomicUsize>,
    statuses: Arc<Mutex<VecDeque<u16>>>,
}

struct Loopback {
    api: RoomHttp,
    capture: Capture,
    task: JoinHandle<()>,
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Loopback {
    async fn start(statuses: &[u16]) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let capture = Capture {
            requests: Arc::new(Mutex::new(Vec::new())),
            attempts: Arc::new(AtomicUsize::new(0)),
            statuses: Arc::new(Mutex::new(statuses.iter().copied().collect())),
        };
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let address = listener.local_addr().expect("loopback address");
        let app = Router::new()
            .fallback(record)
            .layer(DefaultBodyLimit::max(MAX_CAPTURE_BYTES))
            .with_state(capture.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("loopback server");
        });
        let api = RoomHttp::with_origin(
            "synthetic-unused-bot-token".to_owned(),
            format!("http://{address}/api/v10"),
            true,
        )
        .expect("loopback RoomHttp");
        Self { api, capture, task }
    }

    fn assert_attempts(&self, expected: usize) {
        assert_eq!(self.capture.attempts.load(Ordering::Relaxed), expected);
        assert_eq!(
            self.capture.requests.lock().expect("capture").len(),
            expected
        );
        assert!(self.capture.statuses.lock().expect("script").is_empty());
    }
}

async fn record(
    State(capture): State<Capture>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let attempt = capture.attempts.fetch_add(1, Ordering::Relaxed);
    if attempt < MAX_REQUESTS {
        capture.requests.lock().expect("capture").push(RawRequest {
            method,
            path: uri.to_string(),
            content_types: headers
                .get_all("content-type")
                .iter()
                .map(|value| value.to_str().expect("content type").to_owned())
                .collect(),
            authorized: headers.contains_key("authorization"),
            body,
        });
    }
    let status = capture
        .statuses
        .lock()
        .expect("script")
        .pop_front()
        .unwrap_or(500);
    let body = if status == 429 {
        r#"{"retry_after":600,"global":true}"#
    } else if status == 204 {
        ""
    } else {
        "{}"
    };
    Response::builder()
        .status(StatusCode::from_u16(status).expect("script status"))
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("script response")
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("token-only request must not wait for bot backoff or retry")
}

fn review_bytes() -> Vec<u8> {
    // Larger than a Discord message, with byte-sensitive astral and escaped data.
    let long = "room-🛰️-𐐷-\"quoted\"-\\path\r\n".repeat(400);
    serde_json::to_vec_pretty(&json!({
        "schema_version": 1,
        "guild_id": "100",
        "changes": [
            {"path": "creators.400.template", "before": "old", "after": long},
            {"path": "default_limit", "before": 4, "after": 8}
        ]
    }))
    .expect("synthetic review JSON")
}

fn attachment(bytes: &[u8]) -> Attachment {
    let mut file = Attachment::from_bytes(FILENAME.to_owned(), bytes.to_vec(), 0);
    file.description(DESCRIPTION.to_owned());
    file
}

fn buttons() -> Vec<Component> {
    vec![Component::ActionRow(ActionRow {
        id: None,
        components: [
            ("Confirm", CONFIRM, ButtonStyle::Success),
            ("Cancel", CANCEL, ButtonStyle::Secondary),
        ]
        .into_iter()
        .map(|(label, custom_id, style)| {
            Component::Button(Button {
                id: None,
                custom_id: Some(custom_id.to_owned()),
                disabled: false,
                emoji: None,
                label: Some(label.to_owned()),
                style,
                url: None,
                sku_id: None,
            })
        })
        .collect(),
    })]
}

fn deferred() -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::DeferredChannelMessageWithSource,
        data: Some(InteractionResponseData {
            flags: Some(MessageFlags::EPHEMERAL),
            ..Default::default()
        }),
    }
}

fn initial(bytes: &[u8]) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::ChannelMessageWithSource,
        data: Some(InteractionResponseData {
            content: Some(SUMMARY.to_owned()),
            flags: Some(MessageFlags::EPHEMERAL),
            allowed_mentions: Some(AllowedMentions::default()),
            attachments: Some(vec![attachment(bytes)]),
            components: Some(buttons()),
            ..Default::default()
        }),
    }
}

fn assert_route(request: &RawRequest, method: Method, path: &str) {
    assert_eq!(request.method, method);
    assert_eq!(request.path, path);
    assert!(
        !request.authorized,
        "interaction tokens must never acquire bot authorization"
    );
}

fn assert_json(request: &RawRequest) -> Value {
    assert_eq!(request.content_types, ["application/json"]);
    serde_json::from_slice(&request.body).expect("nonempty JSON wire body")
}

fn assert_no_mentions(data: &Value) {
    assert_eq!(data["allowed_mentions"]["parse"], json!([]));
    for key in ["users", "roles"] {
        assert!(data["allowed_mentions"]
            .get(key)
            .is_none_or(|value| value == &json!([])));
    }
    assert_ne!(data["allowed_mentions"]["replied_user"], json!(true));
}

fn assert_review(data: &Value) {
    assert_eq!(
        data["content"], SUMMARY,
        "summary must survive alongside the file"
    );
    assert_eq!(
        data["attachments"],
        json!([{
            "id": 0, "filename": FILENAME, "description": DESCRIPTION
        }])
    );
    assert_eq!(data["components"].as_array().expect("action rows").len(), 1);
    let row = &data["components"][0];
    assert_eq!(row["type"], 1);
    let actual = row["components"].as_array().expect("buttons");
    assert_eq!(actual.len(), 2, "neither Confirm nor Cancel may disappear");
    for (button, label, custom_id, style) in [
        (&actual[0], "Confirm", CONFIRM, 3),
        (&actual[1], "Cancel", CANCEL, 2),
    ] {
        assert_eq!(button["type"], 2);
        assert_eq!(button["label"], label);
        assert_eq!(button["custom_id"], custom_id);
        assert_eq!(button["style"], style);
        assert_ne!(button["disabled"], json!(true));
        assert!(button.get("url").is_none_or(Value::is_null));
    }
    assert_no_mentions(data);
}

fn disposition_parameter<'a>(headers: &'a str, parameter: &str) -> Option<&'a str> {
    let disposition = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-disposition")
            .then_some(value.trim())
    })?;
    assert_eq!(disposition.split(';').next(), Some("form-data"));
    disposition.split(';').skip(1).find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == parameter).then_some(value.trim_matches('"'))
    })
}

fn assert_multipart(request: &RawRequest, expected_bytes: &[u8]) -> Value {
    assert_eq!(
        request.content_types.len(),
        1,
        "exactly one wire content type"
    );
    let mut content_type = request.content_types[0].split(';');
    assert_eq!(
        content_type.next().map(str::trim),
        Some("multipart/form-data"),
        "attachment-bearing RoomHttp requests must send the Twilight form, not an empty JSON body"
    );
    let boundaries: Vec<_> = content_type
        .filter_map(|parameter| {
            let (name, value) = parameter.trim().split_once('=')?;
            (name == "boundary").then_some(value.trim_matches('"'))
        })
        .collect();
    assert_eq!(boundaries.len(), 1, "one declared multipart boundary");
    let boundary = boundaries[0];
    assert!(!boundary.is_empty() && boundary.len() <= 70);
    assert!(boundary.bytes().all(|byte| byte.is_ascii_graphic()));
    let wire = std::str::from_utf8(&request.body).expect("synthetic UTF-8 multipart");
    let wire = wire
        .strip_prefix(&format!("--{boundary}\r\n"))
        .expect("matching first boundary");
    let closing = format!("\r\n--{boundary}--");
    let wire = wire
        .strip_suffix(&format!("{closing}\r\n"))
        .or_else(|| wire.strip_suffix(&closing))
        .expect("matching final boundary");
    let separator = format!("\r\n--{boundary}\r\n");
    let parts: Vec<_> = wire.split(&separator).collect();
    assert_eq!(
        parts.len(),
        2,
        "one payload_json and one files[0], no extras"
    );
    let mut payload = None;
    let mut file = None;
    for part in parts {
        let (headers, body) = part.split_once("\r\n\r\n").expect("multipart headers");
        match disposition_parameter(headers, "name") {
            Some("payload_json") => {
                assert!(payload.is_none(), "no duplicate payload_json");
                payload = Some(serde_json::from_slice(body.as_bytes()).expect("payload JSON"));
            }
            Some("files[0]") => {
                assert!(file.is_none(), "no duplicate files[0]");
                assert_eq!(disposition_parameter(headers, "filename"), Some(FILENAME));
                file = Some(body.as_bytes());
            }
            _ => panic!("unexpected multipart field"),
        }
    }
    assert_eq!(
        file.expect("files[0] exists"),
        expected_bytes,
        "byte-exact complete review JSON"
    );
    payload.expect("payload_json exists")
}

#[tokio::test]
async fn deferred_review_sends_summary_buttons_and_exact_file_in_one_multipart_patch() {
    let mock = Loopback::start(&[204, 200]).await;
    let bytes = review_bytes();
    assert!(bytes.len() > 8_000 && bytes.len() < MAX_CAPTURE_BYTES / 2);
    bounded(
        mock.api
            .respond_interaction(Id::new(1), Id::new(2), TOKEN, &deferred()),
    )
    .await
    .expect("ephemeral defer");
    bounded(mock.api.complete_interaction(
        Id::new(1),
        TOKEN,
        SUMMARY,
        &[attachment(&bytes)],
        Some(&buttons()),
    ))
    .await
    .expect("review completion");
    mock.assert_attempts(2);
    assert!(mock
        .api
        .global_not_before
        .lock()
        .expect("backoff")
        .is_none());
    assert!(!mock.api.is_unauthorized());
    let requests = mock.capture.requests.lock().expect("capture");
    assert_route(&requests[0], Method::POST, CALLBACK);
    assert_eq!(
        assert_json(&requests[0]),
        json!({"type": 5, "data": {"flags": 64}})
    );
    assert_route(&requests[1], Method::PATCH, ORIGINAL);
    assert_review(&assert_multipart(&requests[1], &bytes));
}

#[tokio::test]
async fn initial_review_sends_ephemeral_payload_and_exact_file_in_one_multipart_callback() {
    let mock = Loopback::start(&[204]).await;
    let bytes = review_bytes();
    bounded(
        mock.api
            .respond_interaction(Id::new(1), Id::new(2), TOKEN, &initial(&bytes)),
    )
    .await
    .expect("initial review");
    mock.assert_attempts(1);
    assert!(mock
        .api
        .global_not_before
        .lock()
        .expect("backoff")
        .is_none());
    assert!(!mock.api.is_unauthorized());
    let requests = mock.capture.requests.lock().expect("capture");
    assert_route(&requests[0], Method::POST, CALLBACK);
    let payload = assert_multipart(&requests[0], &bytes);
    assert_eq!(payload["type"], 4);
    assert_eq!(payload["data"]["flags"], 64);
    assert_review(&payload["data"]);
}

#[tokio::test]
async fn json_only_defer_and_edit_stay_json_and_bypass_bot_halt_and_backoff() {
    let mock = Loopback::start(&[204, 200]).await;
    let deadline = Instant::now() + Duration::from_secs(600);
    *mock.api.global_not_before.lock().expect("backoff") = Some(deadline);
    mock.api.unauthorized.store(true, Ordering::Relaxed);
    bounded(
        mock.api
            .respond_interaction(Id::new(1), Id::new(2), TOKEN, &deferred()),
    )
    .await
    .expect("defer ignores bot latch");
    bounded(
        mock.api
            .complete_interaction(Id::new(1), TOKEN, SUMMARY, &[], None),
    )
    .await
    .expect("edit ignores bot latch");
    mock.assert_attempts(2);
    assert_eq!(
        *mock.api.global_not_before.lock().expect("backoff"),
        Some(deadline)
    );
    assert!(
        mock.api.is_unauthorized(),
        "token route must not clear bot halt either"
    );
    let requests = mock.capture.requests.lock().expect("capture");
    assert_route(&requests[0], Method::POST, CALLBACK);
    assert_eq!(
        assert_json(&requests[0]),
        json!({"type": 5, "data": {"flags": 64}})
    );
    assert_route(&requests[1], Method::PATCH, ORIGINAL);
    let edit = assert_json(&requests[1]);
    assert_eq!(edit["content"], SUMMARY);
    assert_no_mentions(&edit);
    assert!(edit.get("attachments").is_none());
    assert!(edit.get("components").is_none());
}

async fn assert_failure_is_single_attempt(status: u16, expected: RoomHttpError) {
    let mock = Loopback::start(&[204, status]).await;
    bounded(
        mock.api
            .respond_interaction(Id::new(1), Id::new(2), TOKEN, &deferred()),
    )
    .await
    .expect("defer before failure");
    let result = bounded(mock.api.complete_interaction(
        Id::new(1),
        TOKEN,
        SUMMARY,
        &[attachment(&review_bytes())],
        Some(&buttons()),
    ))
    .await;
    assert_eq!(result, Err(expected));
    // Keep the recorder live briefly after return to catch detached immediate fallbacks.
    tokio::time::sleep(Duration::from_millis(50)).await;
    mock.assert_attempts(2);
    assert!(mock
        .api
        .global_not_before
        .lock()
        .expect("backoff")
        .is_none());
    assert!(!mock.api.is_unauthorized());
    let requests = mock.capture.requests.lock().expect("capture");
    assert_route(&requests[0], Method::POST, CALLBACK);
    assert_route(&requests[1], Method::PATCH, ORIGINAL);
    // Multipart fidelity is independently required by the two success-path tests.
    // This control isolates retry/followup behavior even on the broken sender.
}

#[tokio::test]
async fn failed_review_completion_never_retries_or_sends_a_confirm_only_fallback() {
    assert_failure_is_single_attempt(500, RoomHttpError::UnknownOutcome).await;
}

#[tokio::test]
async fn rate_limited_review_completion_never_retries_or_sets_bot_global_backoff() {
    assert_failure_is_single_attempt(
        429,
        RoomHttpError::RateLimited {
            retry_after_ms: 600_000,
            global: true,
        },
    )
    .await;
}

#[test]
fn multipart_oracle_accepts_an_independent_two_part_wire_fixture() {
    let bytes = review_bytes();
    let mut body = b"--fixture-boundary\r\nContent-Disposition: form-data; name=\"payload_json\"\r\nContent-Type: application/json\r\n\r\n{\"marker\":true}\r\n--fixture-boundary\r\nContent-Disposition: form-data; name=\"files[0]\"; filename=\"voice-import-review-v1.json\"\r\n\r\n".to_vec();
    body.extend_from_slice(&bytes);
    body.extend_from_slice(b"\r\n--fixture-boundary--\r\n");
    let request = RawRequest {
        method: Method::PATCH,
        path: ORIGINAL.to_owned(),
        content_types: vec!["multipart/form-data; boundary=\"fixture-boundary\"".to_owned()],
        authorized: false,
        body: Bytes::from(body),
    };
    assert_eq!(assert_multipart(&request, &bytes), json!({"marker": true}));
}
