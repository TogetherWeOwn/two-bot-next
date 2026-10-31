use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[derive(Debug, Clone)]
pub struct ScriptedResponse {
    status: u16,
    body: Vec<u8>,
    delay: Duration,
}

impl ScriptedResponse {
    pub fn status(status: u16) -> Self {
        Self {
            status,
            body: Vec::new(),
            delay: Duration::ZERO,
        }
    }

    pub fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            body: body.to_string().into_bytes(),
            delay: Duration::ZERO,
        }
    }

    pub fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

#[derive(Debug, Clone)]
pub struct RestRequest {
    pub method: String,
    pub path: String,
}

pub struct MockRest {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RestRequest>>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl MockRest {
    pub async fn start(script: Vec<ScriptedResponse>, default: ScriptedResponse) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind rest");
        let addr = listener.local_addr().expect("rest addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handle = {
            let requests = Arc::clone(&requests);
            tokio::spawn(async move {
                serve(listener, VecDeque::from(script), default, requests).await;
            })
        };
        Self {
            addr,
            requests,
            handle: Some(handle),
        }
    }

    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn requests(&self) -> Vec<RestRequest> {
        self.requests.lock().expect("recorded requests").clone()
    }

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

async fn serve(
    listener: TcpListener,
    mut script: VecDeque<ScriptedResponse>,
    default: ScriptedResponse,
    requests: Arc<Mutex<Vec<RestRequest>>>,
) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let Some((method, path)) = read_request(&mut stream).await else {
            continue;
        };
        requests
            .lock()
            .expect("recorded requests")
            .push(RestRequest { method, path });
        let response = script.pop_front().unwrap_or_else(|| default.clone());
        if !response.delay.is_zero() {
            tokio::time::sleep(response.delay).await;
        }
        let reason = match response.status {
            200 => "OK",
            204 => "No Content",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Unknown",
        };
        let head = format!(
            "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            response.status,
            reason,
            response.body.len(),
        );
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(&response.body).await;
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
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
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    }
    let head_end = buf.windows(4).position(|window| window == b"\r\n\r\n")?;
    let request_line = String::from_utf8_lossy(&buf[..head_end])
        .lines()
        .next()?
        .to_owned();
    let mut parts = request_line.split_whitespace();
    Some((parts.next()?.to_owned(), parts.next()?.to_owned()))
}

pub fn fresh_message_id(sequence: u64) -> String {
    const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_millis() as u64;
    let created_ms = now_ms - Duration::from_secs(60).as_millis() as u64;
    (((created_ms - DISCORD_EPOCH_MS) << 22) | sequence).to_string()
}
