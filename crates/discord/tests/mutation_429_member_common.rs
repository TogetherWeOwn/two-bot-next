use std::{
    collections::VecDeque,
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
    delay: Duration,
}

impl ScriptedResponse {
    pub fn status(status: u16) -> Self {
        Self {
            status,
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
}

pub struct MockRest {
    addr: std::net::SocketAddr,
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
        let Some(method) = read_request(&mut stream).await else {
            continue;
        };
        requests
            .lock()
            .expect("recorded requests")
            .push(RestRequest { method });
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
            "HTTP/1.1 {} {}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            response.status, reason,
        );
        let _ = stream.write_all(head.as_bytes()).await;
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<String> {
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
    String::from_utf8_lossy(&buf)
        .lines()
        .next()?
        .split_whitespace()
        .next()
        .map(str::to_owned)
}
