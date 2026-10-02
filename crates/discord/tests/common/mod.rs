//! Mock Discord HTTP + gateway double for the S2 prototype ([TOG-9807](/TOG/issues/TOG-9807)).
//!
//! Speaks just enough of the real wire surface (verified against
//! twilight-gateway / twilight-http / twilight-model 0.17.1 sources) for a
//! real `twilight_gateway::Shard` and `twilight_http::Client` to connect,
//! register one guild slash command, receive an interaction, and answer it:
//!
//! - Gateway: plain-`ws://` listener. Text frames pass straight through the
//!   shard (`zlib` only touches binary frames), so the mock sends uncompressed
//!   JSON: `Hello` → reads `Identify` → `READY` dispatch → (on gate)
//!   `INTERACTION_CREATE` dispatch. Heartbeats (`op 1`) get `op 11` ACKs.
//! - HTTP: raw-tokio mini server answering `POST
//!   /api/v10/applications/{app}/guilds/{guild}/commands` with a `Command`
//!   body and `POST /api/v10/interactions/{id}/{token}/callback` with 204,
//!   recording every request for assertions.
//!
//! Dev-only test support: never ships in the release binary.

use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};
use tokio_websockets::{Message, ServerBuilder};

/// Application id the mock pretends to be.
pub const APP_ID: u64 = 1111;
/// Guild id the mock pretends the command lives in.
pub const GUILD_ID: u64 = 2222;
/// Command id echoed back by the mock command-registration endpoint.
pub const COMMAND_ID: u64 = 4444;
/// Interaction id carried by the mock `INTERACTION_CREATE` dispatch.
pub const INTERACTION_ID: u64 = 3333;
/// Interaction token the bot must echo back to the callback endpoint.
pub const INTERACTION_TOKEN: &str = "s2-mock-interaction-token";

const HELLO: &str = r#"{"op":10,"d":{"heartbeat_interval":45000}}"#;
const HEARTBEAT_ACK: &str = r#"{"op":11,"d":null}"#;

/// Minimal `READY` dispatch body: only the fields twilight-model requires
/// (`application`, `guilds`, `resume_gateway_url`, `session_id`, `user`, `v`).
fn ready_payload(gw_addr: &SocketAddr) -> String {
    serde_json::json!({
        "op": 0,
        "s": 1,
        "t": "READY",
        "d": {
            "v": 10,
            "user": {
                "id": "999",
                "username": "s2bot",
                "discriminator": "0",
                "mfa_enabled": false,
            },
            "session_id": "s2-mock-session",
            "resume_gateway_url": format!("ws://{gw_addr}"),
            "guilds": [],
            "application": {"id": APP_ID.to_string(), "flags": 0},
        },
    })
    .to_string()
}

/// Minimal `INTERACTION_CREATE` for `/ping`: twilight-model requires
/// `application_id`, `authorizing_integration_owners`, `id`, `token`, `type`
/// plus `data` for application-command interactions.
fn interaction_create_payload() -> String {
    serde_json::json!({
        "op": 0,
        "s": 2,
        "t": "INTERACTION_CREATE",
        "d": {
            "application_id": APP_ID.to_string(),
            "authorizing_integration_owners": {"0": GUILD_ID.to_string()},
            "id": INTERACTION_ID.to_string(),
            "token": INTERACTION_TOKEN,
            "type": 2,
            "version": 1,
            "guild_id": GUILD_ID.to_string(),
            "data": {
                "id": COMMAND_ID.to_string(),
                "name": "ping",
                "type": 1,
            },
        },
    })
    .to_string()
}

/// `Command` body the mock registration endpoint returns. Includes every
/// field twilight-model's `Command` requires without a default
/// (`default_member_permissions`, `description`, `type`, `name`, `version`).
fn command_body() -> Vec<u8> {
    serde_json::json!({
        "id": COMMAND_ID.to_string(),
        "application_id": APP_ID.to_string(),
        "version": "1",
        "default_member_permissions": null,
        "type": 1,
        "name": "ping",
        "description": "S2 prototype ping",
    })
    .to_string()
    .into_bytes()
}

/// One recorded HTTP request: method, path, and raw body.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

/// The running double: socket addresses plus recorded HTTP traffic.
pub struct MockDiscord {
    /// `ws://` gateway address for `ConfigBuilder::proxy_url`.
    pub gw_addr: SocketAddr,
    /// `host:port` for `ClientBuilder::proxy(_, use_http = true)`.
    pub http_addr: SocketAddr,
    /// Every HTTP request received so far.
    pub recorded: Arc<Mutex<Vec<RecordedRequest>>>,
    interaction_gate: Option<oneshot::Sender<()>>,
    shutdown: Option<oneshot::Sender<()>>,
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl MockDiscord {
    /// Bind both listeners and spawn the background tasks.
    pub async fn start() -> Self {
        let gw_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind gw");
        let http_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind http");
        let gw_addr = gw_listener.local_addr().expect("gw addr");
        let http_addr = http_listener.local_addr().expect("http addr");
        let recorded = Arc::new(Mutex::new(Vec::new()));

        let (gate_tx, gate_rx) = oneshot::channel();
        let (down_tx, down_rx) = oneshot::channel();
        let gw_handle = tokio::spawn(gateway_task(gw_listener, gw_addr, gate_rx, down_rx));
        let http_handle = {
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move { http_task(http_listener, recorded).await })
        };

        Self {
            gw_addr,
            http_addr,
            recorded,
            interaction_gate: Some(gate_tx),
            shutdown: Some(down_tx),
            handles: vec![gw_handle, http_handle],
        }
    }

    /// Snapshot of recorded HTTP requests.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.recorded.lock().expect("recorded").clone()
    }

    /// Tell the gateway double to deliver the `/ping` interaction.
    pub fn fire_interaction(&mut self) {
        if let Some(gate) = self.interaction_gate.take() {
            let _ = gate.send(());
        }
    }

    /// Stop both background tasks.
    pub async fn shutdown(mut self) {
        if let Some(down) = self.shutdown.take() {
            let _ = down.send(());
        }
        for handle in self.handles.drain(..) {
            handle.abort();
        }
    }
}

fn op_of(text: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("op")?.as_i64())
}

/// Gateway side, phase 1: `Hello` → wait for `Identify` → `READY` dispatch.
async fn gateway_task(
    listener: TcpListener,
    gw_addr: SocketAddr,
    gate: oneshot::Receiver<()>,
    done: oneshot::Receiver<()>,
) {
    let (stream, _) = listener.accept().await.expect("gw accept");
    let (_req, mut ws) = ServerBuilder::new()
        .accept(stream)
        .await
        .expect("ws accept");
    ws.send(Message::text(HELLO.to_owned()))
        .await
        .expect("hello");

    tokio::pin!(gate);
    tokio::pin!(done);

    // Phase 1: wait for Identify, answer heartbeats meanwhile.
    loop {
        tokio::select! {
            biased;
            _ = &mut done => return,
            msg = ws.next() => {
                let Some(Ok(msg)) = msg else { return };
                if !msg.is_text() {
                    continue;
                }
                let text = msg.as_text().expect("text").to_owned();
                match op_of(&text) {
                    Some(2) => {
                        ws.send(Message::text(ready_payload(&gw_addr)))
                            .await
                            .expect("ready dispatch");
                        break;
                    }
                    Some(1) => {
                        ws.send(Message::text(HEARTBEAT_ACK.to_owned()))
                            .await
                            .expect("heartbeat ack");
                    }
                    _ => {}
                }
            }
        }
    }

    // Phase 2: wait for the interaction gate (or shutdown), answer heartbeats.
    loop {
        tokio::select! {
            biased;
            _ = &mut done => return,
            _ = &mut gate => {
                ws.send(Message::text(interaction_create_payload()))
                    .await
                    .expect("interaction dispatch");
                break;
            }
            msg = ws.next() => {
                let Some(Ok(msg)) = msg else { return };
                if msg.is_text()
                    && op_of(msg.as_text().expect("text")) == Some(1)
                {
                    ws.send(Message::text(HEARTBEAT_ACK.to_owned()))
                        .await
                        .expect("heartbeat ack");
                }
            }
        }
    }

    // Phase 3: keep ACKing heartbeats until shutdown so the shard stays clean.
    loop {
        tokio::select! {
            biased;
            _ = &mut done => return,
            msg = ws.next() => {
                let Some(Ok(msg)) = msg else { return };
                if msg.is_text()
                    && op_of(msg.as_text().expect("text")) == Some(1)
                {
                    ws.send(Message::text(HEARTBEAT_ACK.to_owned()))
                        .await
                        .expect("heartbeat ack");
                }
            }
        }
    }
}

/// HTTP side: record every request, answer command registration and callbacks.
async fn http_task(listener: TcpListener, recorded: Arc<Mutex<Vec<RecordedRequest>>>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let recorded = Arc::clone(&recorded);
        tokio::spawn(async move { handle_http(stream, recorded).await });
    }
}

async fn handle_http(mut stream: TcpStream, recorded: Arc<Mutex<Vec<RecordedRequest>>>) {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    // Read until the end of the header block.
    loop {
        let Ok(n) = stream.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return;
        }
    }
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("").to_owned();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    let content_len = lines
        .filter_map(|line| line.strip_prefix("content-length:"))
        .filter_map(|v| v.trim().parse::<usize>().ok())
        .next()
        .unwrap_or(0)
        .min(1024 * 1024);
    let mut body = buf[head_end..].to_vec();
    while body.len() < content_len {
        let Ok(n) = stream.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_len);
    recorded.lock().expect("recorded").push(RecordedRequest {
        method,
        path: path.clone(),
        body,
    });

    let (status, payload): (&str, Vec<u8>) = if path.ends_with("/callback") {
        ("204 No Content", Vec::new())
    } else if path.ends_with("/commands") {
        ("200 OK", command_body())
    } else {
        (
            "404 Not Found",
            br#"{"message":"s2 mock: unknown route"}"#.to_vec(),
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        payload.len(),
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.write_all(&payload).await;
}

// ── Scripted REST mock (TOG-10076) ───────────────────────────────────────
// The REST side of the mock Discord double: a plain-HTTP listener that
// replays a scripted response queue (status + headers + body + optional
// delay) while recording every request — method, path, headers, body, and
// mock-side arrival time — for assertions.
//
// The S4 executor points at this via `DISCORD_API_BASE`
// (`ActionExecutor::with_proxy(token, Some(origin))`). Arrival times let
// the acceptance tests pin pacing (110 ms / 350 ms), 429 waits
// (`retry-after + 250 ms`) and 5xx backoff (`500 * 2^attempt`) on the wire
// without trusting client-side timers.
//
// Dev-only test support: never ships in the release binary.

/// One scripted HTTP response: popped in order, one per request.
#[derive(Debug, Clone)]
pub struct ScriptedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Delay before answering (drives the 5 s abort test).
    pub delay: Duration,
}

impl ScriptedResponse {
    /// Bare status with an empty body.
    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            delay: Duration::ZERO,
        }
    }

    /// JSON status with a JSON body.
    pub fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.to_string().into_bytes(),
            delay: Duration::ZERO,
        }
    }

    /// 429 whose JSON `retry_after` body (seconds) wins over `header_secs`
    /// per legacy `kick.ts` — lets tests prove body-beats-header on the wire.
    pub fn rate_limited(body_secs: f64, header_secs: &str) -> Self {
        Self {
            status: 429,
            headers: vec![("retry-after".to_owned(), header_secs.to_owned())],
            body: serde_json::json!({"retry_after": body_secs, "global": false})
                .to_string()
                .into_bytes(),
            delay: Duration::ZERO,
        }
    }

    /// Answer only after `delay` (the abort test uses a delay past 5 s).
    pub fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// One recorded REST request, with the mock-side arrival time.
#[derive(Debug, Clone)]
pub struct RestRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub received_at: std::time::Instant,
}

impl RestRequest {
    /// Case-insensitive header lookup (hyper lowercases wire names).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Synchronize request arrival, response release and handler completion without timers.
#[derive(Default)]
pub struct ResponseGate {
    arrived: tokio::sync::Notify,
    released: tokio::sync::Notify,
    completed: tokio::sync::Notify,
}

impl ResponseGate {
    pub async fn wait_for_request(&self) {
        self.arrived.notified().await;
    }

    pub fn release(&self) {
        self.released.notify_one();
    }

    pub async fn wait_for_completion(&self) {
        self.completed.notified().await;
    }
}

/// A response successfully written to the mock socket, not merely scheduled.
#[derive(Debug, Clone)]
pub struct RestResponse {
    pub path: String,
    pub status: u16,
    pub sent_at: std::time::Instant,
}

type RestResponder =
    Arc<dyn Fn(&RestRequest) -> (ScriptedResponse, Option<Arc<ResponseGate>>) + Send + Sync>;

/// The running scripted REST double.
pub struct MockRest {
    /// Listener address; `origin()` renders the `DISCORD_API_BASE` override.
    pub addr: SocketAddr,
    recorded: Arc<Mutex<Vec<RestRequest>>>,
    responses: Arc<Mutex<Vec<RestResponse>>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl MockRest {
    /// Bind on 127.0.0.1 and start serving `script` in order; once the queue
    /// is spent, every further request gets `default`.
    pub async fn start(script: Vec<ScriptedResponse>, default: ScriptedResponse) -> Self {
        Self::start_with_body_delay(script, default, Duration::ZERO).await
    }

    /// Send headers immediately but hold body bytes to exercise header admission.
    pub async fn start_with_body_delay(
        script: Vec<ScriptedResponse>,
        default: ScriptedResponse,
        body_delay: Duration,
    ) -> Self {
        Self::start_inner(ungated(script), default, Some(body_delay)).await
    }

    /// Send headers but never finish a nonempty body; clients must time out.
    pub async fn start_with_stalled_bodies(
        script: Vec<ScriptedResponse>,
        default: ScriptedResponse,
    ) -> Self {
        Self::start_inner(ungated(script), default, None).await
    }

    /// Hold the final scripted response until the caller releases its gate.
    pub async fn start_gated(
        mut script: Vec<ScriptedResponse>,
        default: ScriptedResponse,
    ) -> (Self, Arc<ResponseGate>) {
        let last = script.pop().expect("a gated response needs a script");
        let gate = Arc::new(ResponseGate::default());
        let mut queue = ungated(script);
        queue.push_back((last, Some(Arc::clone(&gate))));
        (
            Self::start_inner(queue, default, Some(Duration::ZERO)).await,
            gate,
        )
    }

    async fn start_inner(
        script: VecDeque<(ScriptedResponse, Option<Arc<ResponseGate>>)>,
        default: ScriptedResponse,
        body_delay: Option<Duration>,
    ) -> Self {
        let queue = Mutex::new(script);
        Self::serve(
            Arc::new(move |_: &RestRequest| {
                queue
                    .lock()
                    .expect("queue")
                    .pop_front()
                    .unwrap_or_else(|| (default.clone(), None))
            }),
            body_delay,
        )
        .await
    }

    /// Route-aware stateful fixture for concurrent feature orchestration.
    pub async fn with_responder(
        responder: impl Fn(&RestRequest) -> ScriptedResponse + Send + Sync + 'static,
    ) -> Self {
        Self::serve(
            Arc::new(move |request: &RestRequest| (responder(request), None)),
            Some(Duration::ZERO),
        )
        .await
    }

    async fn serve(responder: RestResponder, body_delay: Option<Duration>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind rest");
        let addr = listener.local_addr().expect("rest addr");
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(Vec::new()));
        let handle = {
            let recorded = Arc::clone(&recorded);
            let responses = Arc::clone(&responses);
            tokio::spawn(async move {
                rest_task(listener, recorded, responses, responder, body_delay).await;
            })
        };
        Self {
            addr,
            recorded,
            responses,
            handle: Some(handle),
        }
    }

    /// `http://127.0.0.1:PORT` for `ActionExecutor::with_proxy`.
    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Snapshot of recorded REST requests in arrival order.
    pub fn requests(&self) -> Vec<RestRequest> {
        self.recorded.lock().expect("recorded").clone()
    }

    pub fn responses(&self) -> Vec<RestResponse> {
        self.responses.lock().expect("responses").clone()
    }

    /// Stop the listener and its owned connection tasks.
    pub async fn shutdown(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

fn ungated(
    script: Vec<ScriptedResponse>,
) -> VecDeque<(ScriptedResponse, Option<Arc<ResponseGate>>)> {
    script
        .into_iter()
        .map(|response| (response, None))
        .collect()
}

async fn rest_task(
    listener: TcpListener,
    recorded: Arc<Mutex<Vec<RestRequest>>>,
    responses: Arc<Mutex<Vec<RestResponse>>>,
    responder: RestResponder,
    body_delay: Option<Duration>,
) {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = connections.join_next(), if !connections.is_empty() => continue,
        };
        let Ok((stream, _)) = accepted else { break };
        let recorded = Arc::clone(&recorded);
        let responses = Arc::clone(&responses);
        let responder = Arc::clone(&responder);
        connections.spawn(async move {
            handle_rest(stream, recorded, responses, responder, body_delay).await
        });
    }
}

async fn handle_rest(
    mut stream: TcpStream,
    recorded: Arc<Mutex<Vec<RestRequest>>>,
    responses: Arc<Mutex<Vec<RestResponse>>>,
    responder: RestResponder,
    body_delay: Option<Duration>,
) {
    let Some((method, path, headers, body)) = read_rest_request(&mut stream).await else {
        return;
    };
    let request = RestRequest {
        method: method.clone(),
        path: path.clone(),
        headers,
        body: body.clone(),
        received_at: std::time::Instant::now(),
    };
    let (mut next, gate) = responder(&request);
    recorded.lock().expect("recorded").push(request);
    if let Some(gate) = &gate {
        gate.arrived.notify_one();
        gate.released.notified().await;
    }
    if !next.delay.is_zero() {
        tokio::time::sleep(next.delay).await;
    }
    // A guild-command replace echoes the stored command list back (Discord's
    // PUT contract) and the executor validates that receipt; a scripted bare
    // 200 self-heals into the echo, while an explicit body wins.
    let scripted_body = std::mem::take(&mut next.body);
    let echo_self_heal = next.status == 200
        && method == "PUT"
        && path.ends_with("/commands")
        && scripted_body.is_empty();
    let body = if echo_self_heal { body } else { scripted_body };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        next.status,
        reason_phrase(next.status),
        body.len(),
    );
    for (name, value) in &next.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    if !body.is_empty() && !echo_self_heal {
        let Some(body_delay) = body_delay else {
            // Retain the socket without delivering bytes until the client
            // disconnects. No timer/server-side completion can rescue the test.
            let _ = stream.read(&mut [0u8; 1]).await;
            return;
        };
        if !body_delay.is_zero() {
            tokio::time::sleep(body_delay).await;
        }
    }
    // The echo self-heal above may replace an empty scripted body; deliver
    // whichever body the receipt contract chose.
    if stream.write_all(&body).await.is_ok() {
        responses.lock().expect("responses").push(RestResponse {
            path,
            status: next.status,
            sent_at: std::time::Instant::now(),
        });
    }
    if let Some(gate) = gate {
        gate.completed.notify_one();
    }
}

/// Read one HTTP/1.1 request: request line, all headers, `content-length`
/// body. Returns `None` on a dead or oversized stream.
async fn read_rest_request(
    stream: &mut TcpStream,
) -> Option<(String, String, Vec<(String, String)>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let Ok(n) = stream.read(&mut chunk).await else {
            return None;
        };
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    }
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("").to_owned();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    let mut headers = Vec::new();
    let mut content_len = 0usize;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_lowercase();
        let value = value.trim().to_owned();
        if name == "content-length" {
            content_len = value.parse::<usize>().unwrap_or(0).min(1024 * 1024);
        }
        headers.push((name, value));
    }
    let mut body = buf[head_end..].to_vec();
    while body.len() < content_len {
        let Ok(n) = stream.read(&mut chunk).await else {
            break;
        };
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_len);
    Some((method, path, headers, body))
}
