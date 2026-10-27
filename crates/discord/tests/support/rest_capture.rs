//! Waiver-free REST capture for the seasonal-leaderboard acceptance tests.
//!
//! The shared `tests/common` module is the full mock-Discord double
//! (gateway + scripted REST). A test binary that includes it but drives
//! only the REST side leaves most of it unused, so every consumer today
//! silences `dead_code` on the module. New files cannot add such a waiver
//! (the `CONSTRAINTS.md` floor fails any added `allow` attribute), so this
//! module carries only the surface both acceptance binaries fully use:
//! `MockRest::start`/`origin`/`requests`/`shutdown`,
//! `ScriptedResponse::status`/`json` and the recorded request `body`. Every
//! item, field and method here is live in both binaries; if you add one, use
//! it in both or `clippy -D warnings` fails — that is intentional.
//!
//! Wire behavior mirrors the REST side of `tests/common` for this surface:
//! one HTTP/1.1 request per connection (`content-length` body, recorded in
//! arrival order), scripted responses popped in order with a default once
//! the queue is spent, `connection: close` on every reply. Arrival times,
//! headers, delays and response gates from `common` are deliberately
//! omitted because neither consumer reads them.
//!
//! Dev-only test support: never ships in the release binary.

use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// One scripted HTTP response: popped in order, one per request.
#[derive(Debug, Clone)]
pub struct ScriptedResponse {
    pub status: u16,
    body: Vec<u8>,
}

impl ScriptedResponse {
    /// Bare status with an empty body.
    pub fn status(status: u16) -> Self {
        Self {
            status,
            body: Vec::new(),
        }
    }

    /// Status with a JSON body (the completion-edit mutation receipt).
    pub fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.as_bytes().to_vec(),
        }
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

/// One recorded REST request body, in arrival order.
#[derive(Debug, Clone)]
pub struct RestRequest {
    pub body: Vec<u8>,
}

/// The running scripted REST double.
pub struct MockRest {
    /// Listener address; `origin()` renders the `DISCORD_API_BASE` override.
    addr: SocketAddr,
    recorded: Arc<Mutex<Vec<RestRequest>>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl MockRest {
    /// Bind on 127.0.0.1 and start serving `script` in order; once the queue
    /// is spent, every further request gets `default`.
    pub async fn start(script: Vec<ScriptedResponse>, default: ScriptedResponse) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind rest");
        let addr = listener.local_addr().expect("rest addr");
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let handle = {
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                rest_task(
                    listener,
                    recorded,
                    Mutex::new(script.into_iter().collect()),
                    default,
                )
                .await;
            })
        };
        Self {
            addr,
            recorded,
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

    /// Stop the listener and its owned connection tasks.
    pub async fn shutdown(mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl Drop for MockRest {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

async fn rest_task(
    listener: TcpListener,
    recorded: Arc<Mutex<Vec<RestRequest>>>,
    queue: Mutex<VecDeque<ScriptedResponse>>,
    default: ScriptedResponse,
) {
    let queue = Arc::new(queue);
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = connections.join_next(), if !connections.is_empty() => continue,
        };
        let Ok((stream, _)) = accepted else { break };
        let recorded = Arc::clone(&recorded);
        let queue = Arc::clone(&queue);
        let default = default.clone();
        connections.spawn(async move {
            handle_rest(stream, recorded, queue, default).await;
        });
    }
}

async fn handle_rest(
    mut stream: TcpStream,
    recorded: Arc<Mutex<Vec<RestRequest>>>,
    queue: Arc<Mutex<VecDeque<ScriptedResponse>>>,
    default: ScriptedResponse,
) {
    let Some(body) = read_rest_body(&mut stream).await else {
        return;
    };
    recorded
        .lock()
        .expect("recorded")
        .push(RestRequest { body });
    let next = queue.lock().expect("queue").pop_front().unwrap_or(default);
    let head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        next.status,
        reason_phrase(next.status),
        next.body.len(),
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&next.body).await;
}

/// Read one HTTP/1.1 request and return its `content-length` body. Returns
/// `None` on a dead or oversized stream.
async fn read_rest_body(stream: &mut TcpStream) -> Option<Vec<u8>> {
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
    let mut content_len = 0usize;
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_len = value.trim().parse::<usize>().unwrap_or(0).min(1024 * 1024);
        }
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
    Some(body)
}
