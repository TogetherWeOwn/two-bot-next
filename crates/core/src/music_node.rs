//! Lavalink v4 node client for the staging music spike.
//!
//! Small REST + session-WebSocket client over the in-tree hyper 1 and
//! tokio-websockets stack (see `docs/music-voice-node-spike.md` for why this
//! beats lavalink-rs 0.15 here: same v4 protocol, zero new crates, no new
//! license). It speaks exactly three node operations: open a session (`GET`
//! the WS `/v4/websocket`, read the `ready` op), `PATCH` a player's voice
//! handshake, and `DELETE` the player on leave.
//!
//! The node never holds the bot token: every request carries only the node
//! password (an Operator-set Worker secret) plus the per-guild voice
//! handshake built by [`music_voice`](crate::music_voice). Passwords,
//! session ids and response bodies stay behind [`Secret`](crate::Secret);
//! logs carry op names and status codes only.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderValue, Method, Request, StatusCode};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_websockets::{ClientBuilder, MaybeTlsStream, WebSocketStream};
use url::Url;

use super::music_voice::PlayerVoiceUpdate;
use crate::{Secret, Snowflake};

/// Operator-set node base URL (`http(s)://host:port`, no path): explicit
/// `containerEnvVars` line, never `FORWARDED_FLAGS`, never a committed var.
pub const LAVALINK_URL_VAR: &str = "TWO_LAVALINK_URL";
/// Operator-set node password: explicit `containerEnvVars` line, never
/// `FORWARDED_FLAGS`, never a committed var, never logged.
pub const LAVALINK_PASSWORD_VAR: &str = "TWO_LAVALINK_PASSWORD";

/// Per-operation timeout: the node is container-internal, so 10 s is generous;
/// a slower node fails the spike's join budget loudly instead of wedging it.
pub const NODE_REQUEST_TIMEOUT_SECS: u64 = 10;
/// Largest node response this client buffers. Voice PATCH/DELETE answers are
/// small JSON; anything past 1 MiB is a wrong endpoint, not ours.
pub const NODE_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// Error bodies are truncated to this many chars: enough to name a Lavalink
/// error, never a full dump.
const ERROR_BODY_CHARS: usize = 500;

/// Client name sent on the session WebSocket (`NAME/VERSION` per the v4 docs).
fn client_name() -> String {
    format!("two-bot-next/{}", env!("CARGO_PKG_VERSION"))
}

/// Validated node connection config: base URL plus password.
pub struct NodeConfig {
    base: Url,
    ws_uri: http::Uri,
    auth: HeaderValue,
    password: Secret<String>,
}

// Manual `Debug`: the base URL is safe to print only because validation
// rejects userinfo, so restate that instead of deriving and hoping.
impl fmt::Debug for NodeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeConfig")
            .field("base", &self.base.as_str())
            .field("password", &self.password)
            .finish()
    }
}

/// Why the node config was refused. Values stay redacted: names the missing
/// binding or the violated rule, never the binding's content.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NodeConfigError {
    #[error("TWO_LAVALINK_URL is not set")]
    MissingUrl,
    #[error("TWO_LAVALINK_URL is not a bare http(s) base URL: {reason}")]
    InvalidUrl { reason: Secret<String> },
    #[error("TWO_LAVALINK_PASSWORD is not set")]
    MissingPassword,
    #[error("TWO_LAVALINK_PASSWORD is empty")]
    EmptyPassword,
    #[error("TWO_LAVALINK_PASSWORD cannot travel in an HTTP header")]
    PasswordNotHeaderSafe,
}

impl NodeConfig {
    /// Read the node config from an explicit map (tests, staged config).
    /// Fails closed on anything but a bare `http(s)://host[:port]` base URL
    /// with a non-empty header-safe password.
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self, NodeConfigError> {
        let invalid = |rule: &str| NodeConfigError::InvalidUrl {
            reason: Secret::new(rule.to_owned()),
        };
        let raw_url = vars
            .get(LAVALINK_URL_VAR)
            .ok_or(NodeConfigError::MissingUrl)?;
        let base = Url::parse(raw_url).map_err(|_| invalid("unparseable URL"))?;
        match base.scheme() {
            "http" | "https" => {}
            _ => return Err(invalid("scheme must be http or https")),
        }
        if base.host_str().is_none_or(str::is_empty) {
            return Err(invalid("missing host"));
        }
        // The password travels in the Authorization header only; userinfo in
        // the URL would also land in connector pool keys and error strings.
        if !base.username().is_empty() || base.password().is_some() {
            return Err(invalid("userinfo is forbidden; the password is a header"));
        }
        if base.query().is_some() || base.fragment().is_some() {
            return Err(invalid("query and fragment are forbidden"));
        }
        if !matches!(base.path(), "" | "/") {
            return Err(invalid("only a bare host base URL, no path prefix"));
        }
        let password = vars
            .get(LAVALINK_PASSWORD_VAR)
            .ok_or(NodeConfigError::MissingPassword)?;
        if password.is_empty() {
            return Err(NodeConfigError::EmptyPassword);
        }
        let auth =
            HeaderValue::from_str(password).map_err(|_| NodeConfigError::PasswordNotHeaderSafe)?;
        let mut ws = base.clone();
        ws.set_scheme(if base.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|()| invalid("cannot derive the websocket URL"))?;
        ws.set_path("/v4/websocket");
        let ws_uri: http::Uri = ws
            .as_str()
            .parse()
            .map_err(|_| invalid("websocket URL is not a valid HTTP URI"))?;
        Ok(Self {
            base,
            ws_uri,
            auth,
            password: Secret::new(password.clone()),
        })
    }

    /// Full REST URL for `path` (which starts with `/`).
    fn rest_url(&self, path: &str) -> String {
        format!("{}{path}", self.base.as_str().trim_end_matches('/'))
    }

    /// `PATCH`/`DELETE` URL for `guild_id` on the Lavalink `session`.
    #[must_use]
    pub fn player_url(&self, lavalink_session: &str, guild_id: Snowflake) -> String {
        self.rest_url(&super::music_voice::player_path(lavalink_session, guild_id))
    }

    /// The prebuilt `Authorization` header value (node password).
    #[must_use]
    pub fn auth_value(&self) -> &HeaderValue {
        &self.auth
    }
}

/// Per-operation timeouts. `Default` is the staging spike budget; tests
/// shrink it to prove the timeout paths without waiting them out.
#[derive(Debug, Clone, Copy)]
pub struct NodeTimeouts {
    /// One REST request including its bounded body read.
    pub request: Duration,
    /// One WebSocket connect, and separately the first-frame `ready` wait.
    pub ws: Duration,
}

impl Default for NodeTimeouts {
    fn default() -> Self {
        Self {
            request: Duration::from_secs(NODE_REQUEST_TIMEOUT_SECS),
            ws: Duration::from_secs(NODE_REQUEST_TIMEOUT_SECS),
        }
    }
}

/// What a voice `PATCH` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchOutcome {
    /// The node accepted the handshake (any 2xx).
    Updated,
    /// The node no longer knows the session (404): reconnect the session
    /// WebSocket and retry the `PATCH` once.
    SessionUnknown,
}

/// Why a node operation failed. Carries status codes and fixed rules, never
/// passwords, session ids or full bodies.
#[derive(Debug, Error)]
pub enum NodeError {
    #[error("lavalink node transport failed: {reason}")]
    Transport { reason: Secret<String> },
    #[error("lavalink node operation timed out")]
    Timeout,
    #[error("lavalink node response exceeded {limit} bytes")]
    TooLarge { limit: usize },
    #[error("lavalink node refused the password (401)")]
    Unauthorized,
    #[error("lavalink node returned {status}: {body}")]
    UnexpectedStatus { status: u16, body: Secret<String> },
    #[error("lavalink node websocket failed: {reason}")]
    WsConnect { reason: Secret<String> },
    #[error("lavalink node did not send a ready op")]
    InvalidReady,
    #[error("lavalink node request failed to build")]
    InvalidRequest,
}

/// Hyper-backed Lavalink client. One per forwarder worker; the connector pool
/// inside is shared across its sequential requests.
pub struct NodeClient {
    inner: Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>,
    config: NodeConfig,
    timeouts: NodeTimeouts,
}

impl NodeClient {
    /// Client over `config` with the default staging timeouts.
    #[must_use]
    pub fn new(config: NodeConfig) -> Self {
        let https = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        Self {
            inner: Client::builder(TokioExecutor::new())
                .retry_canceled_requests(false)
                .build(https),
            config,
            timeouts: NodeTimeouts::default(),
        }
    }

    /// Client with explicit timeouts (tests shrink them; the spike keeps default).
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: NodeTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// `PATCH` the player's voice handshake. 2xx is [`PatchOutcome::Updated`];
    /// 404 is [`PatchOutcome::SessionUnknown`] (reconnect, retry once); 401
    /// is a wrong Operator password; anything else is an error carrying the
    /// status and a truncated body.
    pub async fn patch_player_voice(
        &self,
        lavalink_session: &str,
        update: &PlayerVoiceUpdate,
    ) -> Result<PatchOutcome, NodeError> {
        let url = self.config.player_url(lavalink_session, update.guild_id);
        let req = Request::builder()
            .method(Method::PATCH)
            .uri(url)
            .header(
                http::header::AUTHORIZATION,
                self.config.auth_value().clone(),
            )
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(patch_voice_body(update))))
            .map_err(|_| NodeError::InvalidRequest)?;
        let (status, body) = self.send(req).await?;
        match status {
            s if s.is_success() => Ok(PatchOutcome::Updated),
            StatusCode::NOT_FOUND => Ok(PatchOutcome::SessionUnknown),
            StatusCode::UNAUTHORIZED => Err(NodeError::Unauthorized),
            s => Err(NodeError::UnexpectedStatus {
                status: s.as_u16(),
                body: Secret::new(truncate_body(&body)),
            }),
        }
    }

    /// `DELETE` the player. 2xx and 404 (already gone) both succeed; delete
    /// is idempotent, so a rejoin's stale player never blocks the new one.
    pub async fn destroy_player(
        &self,
        lavalink_session: &str,
        guild_id: Snowflake,
    ) -> Result<(), NodeError> {
        let url = self.config.player_url(lavalink_session, guild_id);
        let req = Request::builder()
            .method(Method::DELETE)
            .uri(url)
            .header(
                http::header::AUTHORIZATION,
                self.config.auth_value().clone(),
            )
            .body(Full::new(Bytes::new()))
            .map_err(|_| NodeError::InvalidRequest)?;
        let (status, body) = self.send(req).await?;
        match status {
            s if s.is_success() || s == StatusCode::NOT_FOUND => Ok(()),
            StatusCode::UNAUTHORIZED => Err(NodeError::Unauthorized),
            s => Err(NodeError::UnexpectedStatus {
                status: s.as_u16(),
                body: Secret::new(truncate_body(&body)),
            }),
        }
    }

    /// Open the session WebSocket as `user_id`, read the `ready` op, and hold
    /// the connection open with a drain task: while our side stays connected
    /// the session stays valid for REST. Dropping the session aborts the drain.
    pub async fn connect_session(&self, user_id: Snowflake) -> Result<NodeSession, NodeError> {
        let user_id =
            HeaderValue::from_str(&user_id.to_string()).map_err(|_| NodeError::InvalidRequest)?;
        let client_name =
            HeaderValue::from_str(&client_name()).map_err(|_| NodeError::InvalidRequest)?;
        let builder = ClientBuilder::from_uri(self.config.ws_uri.clone())
            .add_header(
                http::header::AUTHORIZATION,
                self.config.auth_value().clone(),
            )
            .map_err(|_| NodeError::InvalidRequest)?
            .add_header(http::HeaderName::from_static("user-id"), user_id)
            .map_err(|_| NodeError::InvalidRequest)?
            .add_header(http::HeaderName::from_static("client-name"), client_name)
            .map_err(|_| NodeError::InvalidRequest)?;
        let (mut stream, _response) = timeout(self.timeouts.ws, builder.connect())
            .await
            .map_err(|_| NodeError::Timeout)?
            .map_err(|error| NodeError::WsConnect {
                reason: Secret::new(error.to_string()),
            })?;
        let read_ready = async move {
            loop {
                match stream.next().await {
                    // Pings are auto-ponged by the stream but still yielded:
                    // only a text frame advances the handshake.
                    Some(Ok(message)) => {
                        if let Some(text) = message.as_text() {
                            return parse_ready_session(text).map(|id| (id, stream));
                        }
                    }
                    Some(Err(_)) | None => return None,
                }
            }
        };
        let (id, stream) = timeout(self.timeouts.ws, read_ready)
            .await
            .map_err(|_| NodeError::Timeout)?
            .ok_or(NodeError::InvalidReady)?;
        let drain = tokio::spawn(drain_frames(stream));
        Ok(NodeSession {
            id: Secret::new(id),
            drain: Some(drain),
        })
    }

    /// One bounded request: timeout around send plus the capped body read, so
    /// a stalled node fails the operation instead of wedging the worker.
    async fn send(&self, req: Request<Full<Bytes>>) -> Result<(StatusCode, Vec<u8>), NodeError> {
        let fut = async {
            let res = self
                .inner
                .request(req)
                .await
                .map_err(|error| NodeError::Transport {
                    reason: Secret::new(error.to_string()),
                })?;
            let status = res.status();
            let body = Limited::new(res.into_body(), NODE_MAX_RESPONSE_BYTES)
                .collect()
                .await
                .map_err(|error| {
                    if error.downcast_ref::<LengthLimitError>().is_some() {
                        NodeError::TooLarge {
                            limit: NODE_MAX_RESPONSE_BYTES,
                        }
                    } else {
                        NodeError::Transport {
                            reason: Secret::new(error.to_string()),
                        }
                    }
                })?
                .to_bytes()
                .to_vec();
            Ok::<_, NodeError>((status, body))
        };
        timeout(self.timeouts.request, fut)
            .await
            .map_err(|_| NodeError::Timeout)?
    }
}

/// `PATCH` body bytes for a completed handshake.
#[must_use]
pub fn patch_voice_body(update: &PlayerVoiceUpdate) -> Vec<u8> {
    update.voice_body().to_string().into_bytes()
}

/// The Lavalink session id from a `ready` op payload, if this frame is one.
/// Anything else (`stats`, player events, garbage) is `None`: the drain
/// ignores it and the handshake keeps waiting (or refuses a non-ready first
/// text frame).
#[must_use]
pub fn parse_ready_session(payload: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    if value.get("op").and_then(serde_json::Value::as_str) != Some("ready") {
        return None;
    }
    let id = value.get("sessionId").and_then(serde_json::Value::as_str)?;
    if id.is_empty() {
        return None;
    }
    Some(id.to_owned())
}

/// A live node session: the id REST paths are built over, plus the drain task
/// holding the WebSocket open. Dropping aborts the drain (the node expires
/// the orphaned session on its own resume timeout).
pub struct NodeSession {
    id: Secret<String>,
    drain: Option<JoinHandle<()>>,
}

impl NodeSession {
    /// The session id for REST paths. Exposed only into request URLs.
    #[must_use]
    pub fn id(&self) -> &Secret<String> {
        &self.id
    }

    /// False once the drain task has exited (node closed or errored): the
    /// forwarder treats the session as dead and reconnects lazily.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.drain
            .as_ref()
            .is_some_and(|drain| !drain.is_finished())
    }
}

impl fmt::Debug for NodeSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeSession")
            .field("session_id", &self.id)
            .field("live", &self.is_live())
            .finish()
    }
}

impl Drop for NodeSession {
    fn drop(&mut self) {
        if let Some(drain) = self.drain.take() {
            drain.abort();
        }
    }
}

/// Hold the session WebSocket open, logging op names only: frame payloads can
/// carry the session id, so they never reach the logs. Pings are auto-ponged
/// by the stream. Exits on close or read error; the session then reads dead.
async fn drain_frames(stream: WebSocketStream<MaybeTlsStream<TcpStream>>) {
    let mut stream = stream;
    while let Some(result) = stream.next().await {
        match result {
            Ok(message) => {
                if let Some(text) = message.as_text() {
                    tracing::debug!(msg = "music_node_ws_frame", op = ws_op_name(text));
                }
            }
            Err(_) => {
                // Fixed token, never the error value: transport errors echo
                // the peer.
                tracing::debug!(msg = "music_node_ws_closed", cause = "read_error");
                break;
            }
        }
    }
    tracing::debug!(msg = "music_node_ws_drain_exited");
}

/// The op label for a drain log line: known ops verbatim, anything else a
/// fixed token (a malicious node must not get payload text into our logs).
fn ws_op_name(payload: &str) -> &'static str {
    let op = serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("op")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    match op.as_deref() {
        Some("ready") => "ready",
        Some("stats") => "stats",
        Some("playerUpdate") => "playerUpdate",
        Some("event") => "event",
        _ => "unknown",
    }
}

/// Lossy, truncated error-body text for [`NodeError::UnexpectedStatus`].
fn truncate_body(body: &[u8]) -> String {
    String::from_utf8_lossy(body)
        .chars()
        .take(ERROR_BODY_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const PASSWORD: &str = "fixture-node-password";

    fn vars_with(url: &str, password: &str) -> HashMap<String, String> {
        [
            (String::from(LAVALINK_URL_VAR), String::from(url)),
            (String::from(LAVALINK_PASSWORD_VAR), String::from(password)),
        ]
        .into_iter()
        .collect()
    }

    fn config_for(url: &str) -> NodeConfig {
        NodeConfig::from_map(&vars_with(url, PASSWORD)).expect("valid fixture config")
    }

    fn handshake() -> PlayerVoiceUpdate {
        PlayerVoiceUpdate {
            guild_id: 10,
            token: Secret::new(String::from("voice-token")),
            endpoint: String::from("endpoint"),
            session_id: String::from("session"),
            channel_id: 30,
        }
    }

    #[test]
    fn config_accepts_bare_http_and_https_bases() {
        for url in [
            "http://lavalink.internal:2333",
            "http://lavalink.internal:2333/",
            "https://node.example.com",
            "https://node.example.com:443",
            "http://127.0.0.1:8080",
            "http://[::1]:2333",
        ] {
            let config = config_for(url);
            assert_eq!(
                config.player_url("lavalink-session", 10),
                format!(
                    "{}/v4/sessions/lavalink-session/players/10",
                    url.trim_end_matches('/')
                ),
                "player URL joins the base for {url}"
            );
        }
    }

    #[test]
    fn config_refuses_anything_but_a_bare_base() {
        for url in [
            "",
            "not-a-url",
            "ws://node:2333",
            "ftp://node/file",
            "http://user@node:2333",
            "http://user:pass@node:2333",
            "http://node:2333/v4",
            "http://node:2333/v4/",
            "http://node:2333?x=1",
            "http://node:2333#frag",
            "http:///no-host",
            "http://nœud:2333",
        ] {
            assert!(
                matches!(
                    NodeConfig::from_map(&vars_with(url, PASSWORD)),
                    Err(NodeConfigError::InvalidUrl { .. })
                ),
                "{url:?} must be refused"
            );
        }
        assert_eq!(
            NodeConfig::from_map(&vars_with("http://node:2333", "")),
            Err(NodeConfigError::EmptyPassword)
        );
        assert_eq!(
            NodeConfig::from_map(&HashMap::from([(
                String::from(LAVALINK_PASSWORD_VAR),
                String::from(PASSWORD)
            )])),
            Err(NodeConfigError::MissingUrl)
        );
        assert_eq!(
            NodeConfig::from_map(&HashMap::from([(
                String::from(LAVALINK_URL_VAR),
                String::from("http://node:2333")
            )])),
            Err(NodeConfigError::MissingPassword)
        );
        assert_eq!(
            NodeConfig::from_map(&vars_with("http://node:2333", "bad\npassword")),
            Err(NodeConfigError::PasswordNotHeaderSafe)
        );
    }

    #[test]
    fn websocket_url_swaps_scheme_and_path() {
        let http = config_for("http://node:2333");
        assert_eq!(http.ws_uri.to_string(), "ws://node:2333/v4/websocket");
        let https = config_for("https://node.example.com");
        assert_eq!(
            https.ws_uri.to_string(),
            "wss://node.example.com/v4/websocket"
        );
    }

    #[test]
    fn patch_body_carries_the_handshake() {
        let body: serde_json::Value =
            serde_json::from_slice(&patch_voice_body(&handshake())).expect("valid JSON body");
        assert_eq!(body["voice"]["token"], "voice-token");
        assert_eq!(body["voice"]["endpoint"], "endpoint");
        assert_eq!(body["voice"]["sessionId"], "session");
        assert_eq!(body["voice"]["channelId"], "30");
    }

    #[test]
    fn ready_parse_accepts_only_ready_with_session() {
        assert_eq!(
            parse_ready_session(r#"{"op":"ready","resumed":false,"sessionId":"abc"}"#),
            Some(String::from("abc"))
        );
        assert_eq!(
            parse_ready_session(r#"{"op":"ready","resumed":true,"sessionId":"abc"}"#),
            Some(String::from("abc")),
            "a resumed ready still carries a usable id"
        );
        for payload in [
            r#"{"op":"stats","players":1}"#,
            r#"{"op":"event","type":"TrackStartEvent"}"#,
            r#"{"op":"playerUpdate","guildId":"10"}"#,
            r#"{"op":"ready","resumed":false}"#,
            r#"{"op":"ready","resumed":false,"sessionId":""}"#,
            r#"{"op":"ready","resumed":false,"sessionId":12}"#,
            "not json",
            "",
        ] {
            assert_eq!(parse_ready_session(payload), None, "refuse {payload:?}");
        }
    }

    #[test]
    fn debug_and_display_never_leak_secrets() {
        let config = config_for("http://node:2333");
        let output = format!("{config:?}");
        assert!(!output.contains(PASSWORD), "config Debug: {output}");
        assert!(output.contains("[REDACTED]"), "config Debug: {output}");
        for error in [
            NodeError::Transport {
                reason: Secret::new(String::from("fixture-reason")),
            },
            NodeError::WsConnect {
                reason: Secret::new(String::from("fixture-reason")),
            },
            NodeError::UnexpectedStatus {
                status: 500,
                body: Secret::new(String::from("fixture-body")),
            },
            NodeError::Timeout,
            NodeError::Unauthorized,
            NodeError::InvalidReady,
            NodeError::InvalidRequest,
            NodeError::TooLarge { limit: 8 },
        ] {
            let rendered = format!("{error} {error:?}");
            assert!(
                !rendered.contains("fixture-reason") && !rendered.contains("fixture-body"),
                "error leak: {rendered}"
            );
        }
        assert_eq!(
            NodeError::UnexpectedStatus {
                status: 500,
                body: Secret::new(String::from("x")),
            }
            .to_string(),
            "lavalink node returned 500: [REDACTED]"
        );
    }

    #[test]
    fn ws_op_names_allowlist_ops() {
        assert_eq!(ws_op_name(r#"{"op":"ready"}"#), "ready");
        assert_eq!(ws_op_name(r#"{"op":"stats"}"#), "stats");
        assert_eq!(ws_op_name(r#"{"op":"playerUpdate"}"#), "playerUpdate");
        assert_eq!(ws_op_name(r#"{"op":"event"}"#), "event");
        assert_eq!(ws_op_name(r#"{"op":"exploit\",\"x\":\""}"#), "unknown");
        assert_eq!(ws_op_name("garbage"), "unknown");
    }

    #[test]
    fn client_name_is_name_slash_version() {
        let name = client_name();
        let (product, version) = name.split_once('/').expect("NAME/VERSION shape");
        assert_eq!(product, "two-bot-next");
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
    }

    // -- Loopback node doubles below: real HTTP/WS over 127.0.0.1, like the
    // mock Discord gateway. No live Discord, no live node anywhere here. --

    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::{delete, patch};
    use tokio::net::TcpListener;

    #[derive(Debug, Default, Clone)]
    struct Seen {
        auth: Option<String>,
        body: Vec<u8>,
    }

    /// Serve `app` on loopback; returns the base URL plus handles to shut down.
    async fn serve(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let base = format!("http://{}", listener.local_addr().expect("loopback addr"));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("loopback serve");
        });
        (base, task)
    }

    fn test_client(base: &str) -> NodeClient {
        NodeClient::new(config_for(base))
    }

    #[tokio::test]
    async fn patch_sends_auth_and_voice_body() {
        let seen: Arc<Mutex<Seen>> = Arc::new(Mutex::new(Seen::default()));
        let app = axum::Router::new().route(
            "/v4/sessions/{session}/players/{guild}",
            patch(
                |State(seen): State<Arc<Mutex<Seen>>>, headers: HeaderMap, body: Bytes| async move {
                    let mut seen = seen.lock().expect("seen lock");
                    seen.auth = headers
                        .get(http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    seen.body = body.to_vec();
                    StatusCode::NO_CONTENT
                },
            ),
        );
        let (base, server) = serve(app.with_state(seen.clone())).await;
        let outcome = test_client(&base)
            .patch_player_voice("lavalink-session", &handshake())
            .await
            .expect("patch succeeds");
        assert_eq!(outcome, PatchOutcome::Updated);
        server.abort();
        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.auth.as_deref(), Some(PASSWORD));
        let body: serde_json::Value = serde_json::from_slice(&seen.body).expect("JSON body");
        assert_eq!(body["voice"]["channelId"], "30");
        assert_eq!(body["voice"]["sessionId"], "session");
    }

    #[tokio::test]
    async fn patch_maps_statuses() {
        async fn outcome_for(status: StatusCode) -> Result<PatchOutcome, NodeError> {
            let app = axum::Router::new().route(
                "/v4/sessions/{session}/players/{guild}",
                patch(move || async move { status }),
            );
            let (base, server) = serve(app).await;
            let outcome = test_client(&base)
                .patch_player_voice("lavalink-session", &handshake())
                .await;
            server.abort();
            outcome
        }
        assert_eq!(
            outcome_for(StatusCode::OK).await.expect("200 updates"),
            PatchOutcome::Updated
        );
        assert_eq!(
            outcome_for(StatusCode::NOT_FOUND)
                .await
                .expect("404 is not an error"),
            PatchOutcome::SessionUnknown
        );
        assert!(matches!(
            outcome_for(StatusCode::UNAUTHORIZED).await,
            Err(NodeError::Unauthorized)
        ));
        let error = outcome_for(StatusCode::INTERNAL_SERVER_ERROR)
            .await
            .expect_err("500 is an error");
        assert!(matches!(
            error,
            NodeError::UnexpectedStatus { status: 500, .. }
        ));
    }

    #[tokio::test]
    async fn error_body_truncates_and_oversize_refuses() {
        let big = "e".repeat(2000);
        let app = axum::Router::new().route(
            "/v4/sessions/{session}/players/{guild}",
            patch(move || {
                let big = big.clone();
                async move { (StatusCode::BAD_GATEWAY, big) }
            })
            .delete(|| async {
                (
                    StatusCode::BAD_GATEWAY,
                    "e".repeat(NODE_MAX_RESPONSE_BYTES + 1),
                )
            }),
        );
        let (base, server) = serve(app).await;
        let client = test_client(&base);
        let error = client
            .patch_player_voice("lavalink-session", &handshake())
            .await
            .expect_err("502 is an error");
        match error {
            NodeError::UnexpectedStatus { status: 502, body } => {
                assert_eq!(body.expose().len(), ERROR_BODY_CHARS);
            }
            other => panic!("expected truncated 502, got {other:?}"),
        }
        // DELETE shares the send path, so the oversize fence is proven there.
        let error = client
            .destroy_player("lavalink-session", 11)
            .await
            .expect_err("oversize body refuses");
        assert!(matches!(error, NodeError::TooLarge { .. }), "{error:?}");
        server.abort();
    }

    #[tokio::test]
    async fn delete_is_idempotent_on_404() {
        async fn delete_against(status: StatusCode) -> Result<(), NodeError> {
            let app = axum::Router::new().route(
                "/v4/sessions/{session}/players/{guild}",
                delete(move || async move { status }),
            );
            let (base, server) = serve(app).await;
            let outcome = test_client(&base)
                .destroy_player("lavalink-session", 10)
                .await;
            server.abort();
            outcome
        }
        delete_against(StatusCode::OK).await.expect("200 deletes");
        delete_against(StatusCode::NOT_FOUND)
            .await
            .expect("404 deletes (already gone)");
        assert!(matches!(
            delete_against(StatusCode::UNAUTHORIZED).await,
            Err(NodeError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn slow_node_times_out() {
        let app = axum::Router::new().route(
            "/v4/sessions/{session}/players/{guild}",
            patch(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                StatusCode::NO_CONTENT
            }),
        );
        let (base, server) = serve(app).await;
        let client = test_client(&base).with_timeouts(NodeTimeouts {
            request: Duration::from_millis(50),
            ws: Duration::from_millis(50),
        });
        let error = client
            .patch_player_voice("lavalink-session", &handshake())
            .await
            .expect_err("slow node times out");
        assert!(matches!(error, NodeError::Timeout), "{error:?}");
        server.abort();
    }

    use futures_util::SinkExt;
    use tokio_websockets::{Message as WsMessage, ServerBuilder};

    /// Serve one mock session WebSocket: assert the handshake headers, then run
    /// `script` against the accepted stream. Returns the client's outcome.
    async fn with_mock_node<F, Fut>(script: F) -> (NodeSession, Seen)
    where
        F: FnOnce(
                tokio_websockets::WebSocketStream<tokio::net::TcpStream>,
                Arc<Mutex<Seen>>,
            ) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let seen: Arc<Mutex<Seen>> = Arc::new(Mutex::new(Seen::default()));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let base = format!("ws://{}", listener.local_addr().expect("loopback addr"));
        let seen_server = seen.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("ws accept");
            let (request, mut ws) = ServerBuilder::new()
                .accept(stream)
                .await
                .expect("ws upgrade");
            {
                let mut seen = seen_server.lock().expect("seen lock");
                seen.auth = request
                    .headers()
                    .get(http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let mut extra = Vec::new();
                for name in ["user-id", "client-name"] {
                    extra.extend(
                        request
                            .headers()
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .map(|v| format!("{name}={v}")),
                    );
                }
                seen.body = extra.join(",").into_bytes();
            }
            script(ws, seen_server).await;
        });
        let client = test_client(&base.replace("ws://", "http://"));
        let session = client.connect_session(999).await.expect("session connects");
        (session, seen.lock().expect("seen lock").clone())
    }

    #[tokio::test]
    async fn session_handshake_sends_auth_user_and_client() {
        let (_session, seen) = with_mock_node(|mut ws, _| async move {
            ws.send(WsMessage::text(
                r#"{"op":"ready","resumed":false,"sessionId":"node-session-1"}"#,
            ))
            .await
            .expect("ready sends");
            // Keep the session open: the client holds it via the drain task.
            while ws.next().await.is_some() {}
        })
        .await;
        assert_eq!(seen.auth.as_deref(), Some(PASSWORD));
        let headers = String::from_utf8(seen.body).expect("header capture");
        assert!(headers.contains("user-id=999"), "{headers}");
        assert!(
            headers.contains(&format!(
                "client-name=two-bot-next/{}",
                env!("CARGO_PKG_VERSION")
            )),
            "{headers}"
        );
    }

    #[tokio::test]
    async fn session_returns_ready_id_and_ignores_ping_first() {
        let (session, _) = with_mock_node(|mut ws, _| async move {
            ws.send(WsMessage::ping(Bytes::from_static(b"hb")))
                .await
                .expect("ping sends");
            ws.send(WsMessage::text(
                r#"{"op":"ready","resumed":false,"sessionId":"node-session-2"}"#,
            ))
            .await
            .expect("ready sends");
            while ws.next().await.is_some() {}
        })
        .await;
        assert_eq!(session.id().expose(), "node-session-2");
        assert!(session.is_live());
        let rendered = format!("{session:?}");
        assert!(!rendered.contains("node-session-2"), "{rendered}");
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
    }

    #[tokio::test]
    async fn session_refuses_non_ready_first_text() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let base = format!("http://{}", listener.local_addr().expect("loopback addr"));
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("ws accept");
            let (_, mut ws) = ServerBuilder::new()
                .accept(stream)
                .await
                .expect("ws upgrade");
            ws.send(WsMessage::text(r#"{"op":"stats","players":0}"#))
                .await
                .expect("stats sends");
            while ws.next().await.is_some() {}
        });
        let error = test_client(&base)
            .connect_session(999)
            .await
            .expect_err("stats-first is not a session");
        assert!(matches!(error, NodeError::InvalidReady), "{error:?}");
    }

    #[tokio::test]
    async fn session_close_marks_dead_and_drop_closes() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind");
        let base = format!("http://{}", listener.local_addr().expect("loopback addr"));
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<()>();
        let closed_tx = Arc::new(Mutex::new(Some(closed_tx)));
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("ws accept");
            let (_, mut ws) = ServerBuilder::new()
                .accept(stream)
                .await
                .expect("ws upgrade");
            ws.send(WsMessage::text(
                r#"{"op":"ready","resumed":false,"sessionId":"node-session-3"}"#,
            ))
            .await
            .expect("ready sends");
            // First close: the client must notice the session died.
            ws.send(WsMessage::close(None, "going away"))
                .await
                .expect("close sends");
            // Second connection: proves drop aborts the drain (TCP closes).
            let (stream, _) = listener.accept().await.expect("second accept");
            let (_, mut ws) = ServerBuilder::new()
                .accept(stream)
                .await
                .expect("second upgrade");
            ws.send(WsMessage::text(
                r#"{"op":"ready","resumed":false,"sessionId":"node-session-4"}"#,
            ))
            .await
            .expect("second ready sends");
            while ws.next().await.is_some() {}
            if let Some(tx) = closed_tx.lock().expect("closed lock").take() {
                let _ = tx.send(());
            }
        });
        let client = test_client(&base);
        let session = client.connect_session(999).await.expect("session connects");
        tokio::time::timeout(Duration::from_secs(5), async {
            while session.is_live() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drain notices the close");
        assert!(!session.is_live());
        // Drop must abort the drain: the mock sees TCP close promptly.
        let session = client.connect_session(999).await.expect("second session");
        assert!(session.is_live());
        drop(session);
        tokio::time::timeout(Duration::from_secs(5), closed_rx)
            .await
            .expect("drop closes the TCP connection")
            .expect("close signal");
    }

    #[tokio::test]
    async fn dead_node_is_transport_not_timeout() {
        // Port 9 (discard) on loopback: nothing listens, so this refuses fast
        // instead of timing out. Proves transport failures surface distinctly.
        let client = test_client("http://127.0.0.1:9");
        let error = client
            .patch_player_voice("lavalink-session", &handshake())
            .await
            .expect_err("refused REST is a transport error");
        assert!(matches!(error, NodeError::Transport { .. }), "{error:?}");
        let error = client
            .connect_session(999)
            .await
            .expect_err("refused WS is a connect error");
        assert!(matches!(error, NodeError::WsConnect { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn corrupt_session_id_refuses_before_the_wire() {
        // Lavalink session ids are UUID-shaped; a spaced id means our stored
        // id is corrupt, so fail closed without sending anything.
        let app = axum::Router::new().route(
            "/v4/sessions/{session}/players/{guild}",
            patch(|| async { StatusCode::NO_CONTENT }).delete(|| async { StatusCode::NO_CONTENT }),
        );
        let (base, server) = serve(app).await;
        let client = test_client(&base);
        for outcome in [
            client
                .patch_player_voice("not a session", &handshake())
                .await
                .expect_err("spaced PATCH session refuses"),
            client
                .destroy_player("not a session", 10)
                .await
                .expect_err("spaced DELETE session refuses"),
        ] {
            assert!(matches!(outcome, NodeError::InvalidRequest), "{outcome:?}");
        }
        server.abort();
    }
}
