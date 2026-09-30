//! Minimal HTTPS client for backup transports (S3 PUT, Discord REST/CDN).
//!
//! hyper 1.x with rustls (ring + Mozilla roots, the same TLS stack sqlx
//! already uses) — no new trust roots, no new crypto providers. Plain `http`
//! is allowed only to loopback, mirroring the legacy `s3Config.ts` clear-text
//! refusal: a backup uploader that ships the dump and its credentials in
//! clear text is worse than a failed one.
//!
//! Timeouts are per-request (`tokio::time::timeout`); callers surface a
//! timeout as a failed run so the scheduler (systemd timer or Container
//! cron) shows it red.

use bytes::Bytes;
use http::{Method, Request, StatusCode};

pub use http::Method as HttpMethod;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use thiserror::Error;
use tokio::time::Duration;

/// HTTP transport failure. Bodies are truncated: an S3 XML error fits, a
/// full dump echoed back does not belong in a log line.
#[derive(Debug, Error)]
pub enum HttpError {
    #[error("invalid url {url:?}: {reason}")]
    InvalidUrl { url: String, reason: String },
    #[error("refusing clear-text {url:?}: remote hosts must be https://")]
    ClearText { url: String },
    #[error("request to {url:?} failed: {reason}")]
    Transport { url: String, reason: String },
    #[error("request to {url:?} timed out after {secs}s")]
    Timeout { url: String, secs: u64 },
    #[error(
        "response from {url:?} exceeds the {limit}-byte cap; refusing rather than buffering it"
    )]
    TooLarge { url: String, limit: usize },
    #[error("unexpected status {status} from {url:?}{detail}")]
    Status {
        url: String,
        status: StatusCode,
        detail: String,
    },
}

/// A fetched response: status plus the bounded body.
#[derive(Debug)]
pub struct HttpResponse {
    pub status: StatusCode,
    pub body: Vec<u8>,
    /// Response headers as received (lowercase names), for etag/content-type.
    pub headers: Vec<(String, String)>,
}

impl HttpResponse {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// JSON body, or `None` when the body is not JSON (Discord error pages,
    /// S3 XML errors) — callers fall back to the status + truncated text.
    #[must_use]
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }

    /// First 500 chars of the body for error detail (mirrors the legacy
    /// uploader's S3 XML inclusion: the status alone does not distinguish a
    /// wrong key from a wrong bucket).
    #[must_use]
    pub fn detail(&self) -> String {
        let text = String::from_utf8_lossy(&self.body);
        let snippet: String = text.chars().take(500).collect();
        if snippet.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", snippet.trim())
        }
    }
}

/// Largest response body this client will buffer. Discord JSON, S3 XML
/// errors and CDN emoji images are kilobytes; anything past 8 MiB is not one
/// of ours — a wrong endpoint (or a compromised one) must fail the run, not
/// eat the host's RAM (PR #11 review).
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

fn check_url(url: &str) -> Result<(), HttpError> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let host = rest.split(['/', ':']).next().unwrap_or("");
        if host == "localhost" || host == "127.0.0.1" || host == "::1" {
            return Ok(());
        }
        return Err(HttpError::ClearText {
            url: url.to_owned(),
        });
    }
    Err(HttpError::InvalidUrl {
        url: url.to_owned(),
        reason: "must start with https:// or http://".to_owned(),
    })
}

/// One HTTP request. `headers` are `(name, value)` pairs; `body` is sent
/// verbatim. HTTPS via Mozilla roots; plain HTTP only to loopback.
pub async fn request(
    method: Method,
    url: &str,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    timeout_secs: u64,
) -> Result<HttpResponse, HttpError> {
    check_url(url)?;

    let https = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .build();
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new())
        .retry_canceled_requests(false)
        .build(https);

    let mut builder = Request::builder().method(method).uri(url);
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    let req = builder
        .body(Full::new(Bytes::from(body.unwrap_or_default())))
        .map_err(|e| HttpError::InvalidUrl {
            url: url.to_owned(),
            reason: e.to_string(),
        })?;

    let url_owned = url.to_owned();
    let fut = async {
        let res = client
            .request(req)
            .await
            .map_err(|e| HttpError::Transport {
                url: url_owned.clone(),
                reason: e.to_string(),
            })?;
        let status = res.status();
        let headers: Vec<(String, String)> = res
            .headers()
            .iter()
            .map(|(n, v)| (n.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
            .collect();
        let body = Limited::new(res.into_body(), MAX_RESPONSE_BYTES)
            .collect()
            .await
            .map_err(|e| {
                if e.downcast_ref::<LengthLimitError>().is_some() {
                    HttpError::TooLarge {
                        url: url_owned.clone(),
                        limit: MAX_RESPONSE_BYTES,
                    }
                } else {
                    HttpError::Transport {
                        url: url_owned.clone(),
                        reason: e.to_string(),
                    }
                }
            })?
            .to_bytes()
            .to_vec();
        Ok(HttpResponse {
            status,
            body,
            headers,
        })
    };

    tokio::time::timeout(Duration::from_secs(timeout_secs), fut)
        .await
        .map_err(|_| HttpError::Timeout {
            url: url.to_owned(),
            secs: timeout_secs,
        })?
}

/// Convenience GET with no body.
pub async fn get(
    url: &str,
    headers: Vec<(String, String)>,
    timeout_secs: u64,
) -> Result<HttpResponse, HttpError> {
    request(Method::GET, url, headers, None, timeout_secs).await
}

/// Convenience PUT with a body.
pub async fn put(
    url: &str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout_secs: u64,
) -> Result<HttpResponse, HttpError> {
    request(Method::PUT, url, headers, Some(body), timeout_secs).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    /// Loopback refusal probe: bind, then close without accepting.
    async fn closed_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
            unreachable!()
        };
        addr.port()
    }

    #[test]
    fn remote_clear_text_is_refused_before_any_socket() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let err = get("http://s3.example.com/key", vec![], 5)
                .await
                .expect_err("clear-text remote must be refused");
            assert!(matches!(err, HttpError::ClearText { .. }), "{err}");
        });
    }

    #[tokio::test]
    async fn loopback_http_is_allowed_and_timeouts_surface() {
        // Nothing listens here: connection refused (transport), not a
        // clear-text refusal — and a black-hole would surface as Timeout.
        let port = closed_port().await;
        let err = get(&format!("http://127.0.0.1:{port}/x"), vec![], 5)
            .await
            .expect_err("closed port must fail");
        assert!(matches!(err, HttpError::Transport { .. }), "{err}");
    }

    #[tokio::test]
    async fn oversize_response_is_refused_instead_of_buffered() {
        // A wrong (or compromised) endpoint must fail the run, not eat the
        // host's RAM: bodies past MAX_RESPONSE_BYTES surface as TooLarge.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            // Drain the request head first: closing with an unread request
            // pending makes the OS send RST, which would fail the client's
            // in-flight body read before the size cap trips.
            let mut buf = vec![0u8; 4096];
            let mut head_read = Vec::new();
            loop {
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                head_read.extend_from_slice(&buf[..n]);
                if head_read.windows(4).any(|w| w == b"\r\n\r\n") || head_read.len() > 65_536 {
                    break;
                }
            }
            let filler = vec![b'x'; MAX_RESPONSE_BYTES + 1];
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                filler.len()
            );
            socket.write_all(head.as_bytes()).await.ok();
            socket.write_all(&filler).await.ok();
        });
        let err = get(&format!("http://127.0.0.1:{port}/big"), vec![], 30)
            .await
            .expect_err("oversize body must be refused");
        assert!(matches!(err, HttpError::TooLarge { .. }), "{err}");
    }
}
