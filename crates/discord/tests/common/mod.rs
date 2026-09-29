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
    net::SocketAddr,
    sync::{Arc, Mutex},
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
