use super::*;
use serde_json::json;
use std::{io, sync::Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

const CHANNEL: &str = "333333333333333333";
const MESSAGE: &str = "444444444444444444";

#[derive(Clone)]
struct Reply {
    status: u16,
    headers: String,
    body: String,
    stall_response: bool,
    stall_body: bool,
    truncate: bool,
    disconnect: bool,
}

impl Reply {
    fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: String::new(),
            body: body.into(),
            stall_response: false,
            stall_body: false,
            truncate: false,
            disconnect: false,
        }
    }

    fn success() -> Self {
        Self::new(
            200,
            json!({"id": MESSAGE, "channel_id": CHANNEL}).to_string(),
        )
    }
}

struct Recorded {
    method: String,
    path: String,
    user_agent: Option<String>,
    body: Value,
}

struct MockDiscord {
    origin: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    request_received: Arc<tokio::sync::Notify>,
    response_received: Arc<tokio::sync::Notify>,
    stall_response: bool,
    task: JoinHandle<()>,
    _clock_hold: std::sync::mpsc::Sender<()>,
}

impl MockDiscord {
    async fn start(reply: Reply) -> Self {
        // Paused time normally auto-advances while real sockets wait on the
        // reactor. A live blocking task inhibits that documented Tokio behavior,
        // without spinning or tying the HTTP deadline to host scheduling speed.
        let (clock_hold, release) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let _ = started.send(());
            let _ = release.recv();
        });
        ready.await.unwrap();
        let request_received = Arc::new(tokio::sync::Notify::new());
        let response_received = Arc::new(tokio::sync::Notify::new());
        let received = request_received.clone();
        let stall_response = reply.stall_response;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                let (head_end, content_length) = loop {
                    let count = socket.read(&mut chunk).await.unwrap();
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    assert!(bytes.len() < 64 * 1024, "test request exceeded limit");
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let head = std::str::from_utf8(&bytes[..end]).unwrap();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        break (end + 4, length);
                    }
                };
                while bytes.len() < head_end + content_length {
                    let count = socket.read(&mut chunk).await.unwrap();
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                }
                let head = std::str::from_utf8(&bytes[..head_end]).unwrap();
                let mut first_line = head.lines().next().unwrap().split_whitespace();
                seen.lock().unwrap().push(Recorded {
                    method: first_line.next().unwrap().to_owned(),
                    path: first_line.next().unwrap().to_owned(),
                    user_agent: head.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("user-agent")
                            .then(|| value.trim().to_owned())
                    }),
                    body: serde_json::from_slice(&bytes[head_end..head_end + content_length])
                        .unwrap(),
                });
                received.notify_one();
                if reply.disconnect {
                    continue;
                }
                if reply.stall_response {
                    std::future::pending::<()>().await;
                }
                let response = format!(
                    "HTTP/1.1 {} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{}connection: close\r\n\r\n",
                    reply.status, reply.body.len(), reply.headers,
                );
                let _ = socket.write_all(response.as_bytes()).await;
                if reply.stall_body {
                    std::future::pending::<()>().await;
                }
                let end = if reply.truncate {
                    reply.body.len() / 2
                } else {
                    reply.body.len()
                };
                let _ = socket.write_all(&reply.body.as_bytes()[..end]).await;
            }
        });
        Self {
            origin,
            requests,
            request_received,
            response_received,
            stall_response,
            task,
            _clock_hold: clock_hold,
        }
    }

    fn executor(&self, keys: HashMap<String, String>) -> AnnouncementExecutor {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // This is a generated, non-credential local fixture. Never read env tokens.
        let client = TwilightClient::builder()
            .token(format!("local-fixture-{}", std::process::id()))
            .build();
        let mut executor = AnnouncementExecutor::new(Arc::new(client), keys);
        executor.api_origin = self.origin.clone();
        executor.timeout = Duration::from_millis(100);
        executor.response_received = Some(self.response_received.clone());
        executor
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for MockDiscord {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn keys() -> HashMap<String, String> {
    two_bot_core::internal_actions::build_channel_keys(&format!("ann:{CHANNEL}")).unwrap()
}

fn payload(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn announcement(content: &str) -> Map<String, Value> {
    payload(json!({"channel_key": "ann", "body": content}))
}

async fn run_once(executor: &AnnouncementExecutor, body: &Map<String, Value>) -> ExecutionOutcome {
    let start = tokio::time::Instant::now();
    let outcome = executor.execute("announcement.post", body).await;
    assert_eq!(
        start.elapsed(),
        Duration::ZERO,
        "socket I/O advanced the clock"
    );
    outcome
}

async fn run_until_timeout(
    mock: &MockDiscord,
    executor: &AnnouncementExecutor,
    body: &Map<String, Value>,
) -> ExecutionOutcome {
    let ready = if mock.stall_response {
        &mock.request_received
    } else {
        &mock.response_received
    };
    let pending = executor.execute("announcement.post", body);
    tokio::pin!(pending);
    tokio::select! {
        outcome = &mut pending => panic!("stalled request finished before its deadline: {outcome:?}"),
        () = ready.notified() => {}
    }
    // The fixture has received the request (or the executor has acquired the
    // headers). Only now cross the deadline; kernel I/O cannot race virtual time.
    let tick = Duration::from_millis(1);
    tokio::time::advance(executor.timeout - tick).await;
    assert!(futures_util::poll!(&mut pending).is_pending());
    tokio::time::advance(tick + tick).await;
    pending.await
}

#[tokio::test(start_paused = true)]
async fn twilight_posts_exact_mapped_route_and_mention_safe_payload() {
    let mock = MockDiscord::start(Reply::success()).await;
    let executor = mock.executor(keys());
    let text = "news é 🦀 می\u{200c}روم @eve\u{200c}ryone @here <@&123456789012345678> <@123456789012345678>";
    let mut body = announcement(text);
    body.insert("channel_id".to_owned(), json!("999999999999999999"));
    body.insert(
        "access_token".to_owned(),
        json!("never_forward_oauth_material"),
    );
    let outcome = run_once(&executor, &body).await;
    let ExecutionOutcome::Posted(receipt) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(receipt.channel_id().to_string(), CHANNEL);
    assert_eq!(receipt.message_id().to_string(), MESSAGE);
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(
        requests[0].user_agent.as_deref(),
        Some(concat!(
            "DiscordBot (https://github.com/TogetherWeOwn/two-bot-next, ",
            env!("CARGO_PKG_VERSION"),
            ")"
        ))
    );
    assert_eq!(
        requests[0].path,
        format!("/api/v10/channels/{CHANNEL}/messages")
    );
    assert_eq!(
        requests[0].body,
        json!({
            "content": "news é 🦀 می\u{200c}روم @\u{200b}everyone @\u{200b}here <@&123456789012345678> <@123456789012345678>",
            "allowed_mentions": {"parse": []}
        })
    );
    assert_eq!(body["body"], text);
    assert_eq!(
        serde_json::to_value(outcome).unwrap(),
        json!({"posted": {"channel_id": CHANNEL, "message_id": MESSAGE}})
    );
}

#[tokio::test(start_paused = true)]
async fn refuses_bad_inputs_missing_mapping_and_every_other_core_verb_without_http() {
    let mock = MockDiscord::start(Reply::success()).await;
    let executor = mock.executor(keys());
    assert_eq!(SUPPORTED_ACTIONS, ["announcement.post"]);
    for action in two_bot_core::internal_actions::IMPLEMENTED_ACTIONS {
        if action == "announcement.post" {
            continue;
        }
        assert!(!AnnouncementExecutor::supports(action));
        assert_eq!(
            executor.execute(action, &announcement("ok")).await,
            ExecutionOutcome::NoEffect(Refusal::ActionNotAllowed)
        );
    }
    assert_eq!(
        executor
            .execute("attacker-controlled-verb", &Map::new())
            .await,
        ExecutionOutcome::NoEffect(Refusal::ActionNotAllowed)
    );
    for value in [
        json!({}),
        json!({"channel_key": "ann"}),
        json!({"channel_key": "ann", "body": ""}),
        json!({"channel_key": "ann", "body": "\u{200b}"}),
        json!({"channel_key": "ann", "body": "\u{200c}"}),
        json!({"channel_key": "ann", "body": "\u{feff}"}),
        json!({"channel_key": 123, "body": "ok"}),
        json!({"channel_key": "ann", "body": 123}),
        json!({"channel_key": "ann", "body": "a".repeat(2001)}),
        json!({"channel_key": "ann", "body": "🦀".repeat(1001)}),
    ] {
        assert_eq!(
            run_once(&executor, &payload(value)).await,
            ExecutionOutcome::NoEffect(Refusal::Malformed)
        );
    }
    assert_eq!(
        run_once(
            &executor,
            &payload(json!({"channel_key": "unknown-secret-text", "body": "ok"}))
        )
        .await,
        ExecutionOutcome::NoEffect(Refusal::ActionNotAllowed)
    );
    let empty = mock.executor(HashMap::new());
    assert_eq!(
        run_once(&empty, &announcement("ok")).await,
        ExecutionOutcome::NoEffect(Refusal::ActionNotAllowed)
    );
    for bad_id in ["0", "00000000000000000", "abc", "99999999999999999999"] {
        let invalid = mock.executor(HashMap::from([("ann".to_owned(), bad_id.to_owned())]));
        assert_eq!(
            run_once(&invalid, &announcement("ok")).await,
            ExecutionOutcome::NoEffect(Refusal::InvalidChannelConfiguration)
        );
    }
    assert_eq!(mock.count(), 0);
}

#[tokio::test(start_paused = true)]
async fn utf16_ceiling_is_preserved_for_non_ascii_content() {
    let mock = MockDiscord::start(Reply::success()).await;
    let executor = mock.executor(keys());
    for text in ["é".repeat(2000), "🦀".repeat(1000)] {
        assert!(matches!(
            run_once(&executor, &announcement(&text)).await,
            ExecutionOutcome::Posted(_)
        ));
    }
    assert_eq!(mock.count(), 2);
}

#[tokio::test(start_paused = true)]
async fn definite_discord_rejections_and_rate_limit_are_one_attempt() {
    for status in [400, 401, 403, 404, 405, 413, 415, 422, 429] {
        // Error content is deliberately malformed and sensitive; status alone
        // proves rejection, without decoding/echoing the provider body.
        let mock = MockDiscord::start(Reply::new(status, "provider-secret-not-json")).await;
        let executor = mock.executor(keys());
        let outcome = if status == 429 {
            ExecutionOutcome::RateLimited(RateLimitCooldown {
                scope: CooldownScope::Global,
                retry_after_ms: None,
            })
        } else {
            ExecutionOutcome::NoEffect(Refusal::DiscordRejected)
        };
        assert_eq!(
            run_once(&executor, &announcement("private-message")).await,
            outcome
        );
        assert_eq!(mock.count(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn rate_limits_preserve_timing_and_conservative_scope_for_shared_governor() {
    let channel = CooldownScope::Channel(Id::new(CHANNEL.parse().unwrap()));
    for (headers, body, scope, delay) in [
        (
            "Retry-After: 65\r\nX-RateLimit-Scope: user\r\n",
            r#"{"retry_after":64.57,"global":false}"#,
            channel,
            Some(65_000),
        ),
        (
            "Retry-After: 1\r\n",
            r#"{"retry_after":1.2345,"global":false}"#,
            channel,
            Some(1235),
        ),
        (
            "",
            r#"{"retry_after":0.5,"global":true}"#,
            CooldownScope::Global,
            Some(500),
        ),
        (
            "Retry-After: 2\r\nX-RateLimit-Global: true\r\n",
            r#"{"global":false}"#,
            CooldownScope::Global,
            Some(2000),
        ),
        (
            "Retry-After: 3\r\nX-RateLimit-Scope: global\r\n",
            "not-json",
            CooldownScope::Global,
            Some(3000),
        ),
        (
            "Retry-After: 4\r\nX-RateLimit-Scope: shared\r\n",
            "not-json",
            channel,
            Some(4000),
        ),
        (
            "Retry-After: NaN\r\n",
            r#"{"retry_after":-1}"#,
            CooldownScope::Global,
            None,
        ),
        (
            "Retry-After: inf\r\n",
            r#"{"retry_after":1e100}"#,
            CooldownScope::Global,
            None,
        ),
        (
            "Retry-After: 86400\r\n",
            "{}",
            CooldownScope::Global,
            Some(86_400_000),
        ),
    ] {
        let mut reply = Reply::new(429, body);
        reply.headers = format!("{headers}X-RateLimit-Bucket: private-provider-bucket\r\n");
        let mock = MockDiscord::start(reply).await;
        let executor = mock.executor(keys());
        let outcome = run_once(&executor, &announcement("private-message")).await;
        assert_eq!(
            outcome,
            ExecutionOutcome::RateLimited(RateLimitCooldown {
                scope,
                retry_after_ms: delay,
            })
        );
        assert_eq!(mock.count(), 1);
        let safe = format!("{outcome:?} {}", serde_json::to_string(&outcome).unwrap());
        assert!(!safe.contains("private-provider-bucket"));
        assert!(!safe.contains(body));
    }
}

#[tokio::test(start_paused = true)]
async fn broken_rate_limit_bodies_retain_headers_and_definite_no_effect() {
    let mut slow = Reply::new(429, r#"{"retry_after":1,"global":true}"#);
    slow.stall_body = true;
    let mut truncated = slow.clone();
    truncated.stall_body = false;
    truncated.truncate = true;
    let oversized = Reply::new(429, "x".repeat(MAX_RESPONSE_BYTES + 1));
    for mut reply in [slow, truncated, oversized] {
        reply.headers = "Retry-After: 6.5\r\nX-RateLimit-Global: true\r\n".to_owned();
        let stalled = reply.stall_body;
        let mock = MockDiscord::start(reply).await;
        let executor = mock.executor(keys());
        let outcome = if stalled {
            run_until_timeout(&mock, &executor, &announcement("ok")).await
        } else {
            run_once(&executor, &announcement("ok")).await
        };
        assert_eq!(
            outcome,
            ExecutionOutcome::RateLimited(RateLimitCooldown {
                scope: CooldownScope::Global,
                retry_after_ms: Some(6500),
            })
        );
        assert_eq!(mock.count(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn redirects_request_timeout_and_server_errors_remain_unknown_without_retry() {
    for status in [202, 204, 301, 307, 408, 500, 502, 503, 504] {
        let mock = MockDiscord::start(Reply::new(status, "provider-secret")).await;
        let executor = mock.executor(keys());
        assert_eq!(
            run_once(&executor, &announcement("private-message")).await,
            ExecutionOutcome::Unknown(UnknownReason::Upstream)
        );
        assert_eq!(mock.count(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn timeout_and_lost_response_are_unknown_and_not_retried() {
    let mut delayed = Reply::success();
    delayed.stall_response = true;
    let mock = MockDiscord::start(delayed).await;
    let executor = mock.executor(keys());
    assert_eq!(
        run_until_timeout(&mock, &executor, &announcement("private-message")).await,
        ExecutionOutcome::Unknown(UnknownReason::Timeout)
    );
    assert_eq!(mock.count(), 1);

    let mut lost = Reply::success();
    lost.disconnect = true;
    let mock = MockDiscord::start(lost).await;
    let executor = mock.executor(keys());
    assert_eq!(
        run_once(&executor, &announcement("private-message")).await,
        ExecutionOutcome::Unknown(UnknownReason::Transport)
    );
    assert_eq!(mock.count(), 1);
}

#[tokio::test(start_paused = true)]
async fn invalid_success_receipts_are_unknown_instead_of_cached_failures() {
    for response in [
        "not-json".to_owned(),
        json!({"id": MESSAGE}).to_string(),
        json!({"id": "0", "channel_id": CHANNEL}).to_string(),
        json!({"id": "99999999999999999999", "channel_id": CHANNEL}).to_string(),
        json!({"id": MESSAGE, "channel_id": "555555555555555555"}).to_string(),
        json!({"id": 444444444444444444_u64, "channel_id": CHANNEL}).to_string(),
        "x".repeat(MAX_RESPONSE_BYTES + 1),
    ] {
        let mock = MockDiscord::start(Reply::new(200, response)).await;
        let executor = mock.executor(keys());
        assert_eq!(
            run_once(&executor, &announcement("private-message")).await,
            ExecutionOutcome::Unknown(UnknownReason::InvalidResponse)
        );
        assert_eq!(mock.count(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn deadline_covers_success_body_and_truncated_body_is_unknown() {
    let mut slow_body = Reply::success();
    slow_body.stall_body = true;
    let mock = MockDiscord::start(slow_body).await;
    let executor = mock.executor(keys());
    assert_eq!(
        run_until_timeout(&mock, &executor, &announcement("ok")).await,
        ExecutionOutcome::Unknown(UnknownReason::Timeout)
    );
    assert_eq!(mock.count(), 1);

    let mut truncated = Reply::success();
    truncated.truncate = true;
    let mock = MockDiscord::start(truncated).await;
    let executor = mock.executor(keys());
    assert_eq!(
        run_once(&executor, &announcement("ok")).await,
        ExecutionOutcome::Unknown(UnknownReason::InvalidResponse)
    );
    assert_eq!(mock.count(), 1);
}

#[tokio::test(start_paused = true)]
async fn missing_authentication_is_local_no_effect() {
    let mock = MockDiscord::start(Reply::success()).await;
    let mut executor = mock.executor(keys());
    executor.twilight = Arc::new(TwilightClient::builder().build());
    assert_eq!(
        run_once(&executor, &announcement("ok")).await,
        ExecutionOutcome::NoEffect(Refusal::LocalConfiguration)
    );
    assert_eq!(mock.count(), 0);
}

#[derive(Clone)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn outcomes_debug_serialization_and_logs_never_echo_sensitive_values() {
    let logs = LogBuffer(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .without_time()
        .with_writer(logs.clone())
        .finish();
    // One global capture for this test binary keeps callsite interest stable
    // while the other executor tests emit concurrently on different threads.
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let provider_text = "private-provider-response";
    let response = json!({"id": MESSAGE, "channel_id": CHANNEL, "content": provider_text, "access_token": "private-oauth-response"});
    let mock = MockDiscord::start(Reply::new(200, response.to_string())).await;
    let executor = mock.executor(keys());
    let mut body = announcement("private-request-body");
    body.insert("access_token".to_owned(), json!("private-oauth-request"));
    let posted = run_once(&executor, &body).await;
    let refused = run_once(
        &executor,
        &payload(json!({"channel_key": "private-channel-key", "body": "private-request-body"})),
    )
    .await;
    let mut rate_reply = Reply::new(
        429,
        r#"{"retry_after":2,"global":true,"message":"private-rate-message"}"#,
    );
    rate_reply.headers = "X-RateLimit-Bucket: private-provider-bucket\r\n".to_owned();
    let rate_mock = MockDiscord::start(rate_reply).await;
    let rate_executor = rate_mock.executor(keys());
    let limited = run_once(&rate_executor, &body).await;
    assert!(matches!(limited, ExecutionOutcome::RateLimited(_)));
    let output = format!(
        "{posted:?} {refused:?} {limited:?} {} {} {} {}",
        serde_json::to_string(&limited).unwrap(),
        serde_json::to_string(&posted).unwrap(),
        serde_json::to_string(&refused).unwrap(),
        String::from_utf8(logs.0.lock().unwrap().clone()).unwrap()
    );
    assert!(output.contains("internal action completed"));
    for forbidden in [
        "private-request-body",
        "private-provider-response",
        "private-oauth-response",
        "private-oauth-request",
        "private-channel-key",
        "private-rate-message",
        "private-provider-bucket",
        &format!("local-fixture-{}", std::process::id()),
    ] {
        assert!(
            !output.contains(forbidden),
            "sensitive value escaped safe contract"
        );
    }
}
