//! Fetch the Worker's `GET /ops/config` record for the settings poll (Slice 0a).
//!
//! The poll reads [`OPS_CONFIG_URL_ENV`] on every tick and GETs the URL
//! as-is: no path suffix, no query added. [`OPS_CONFIG_TOKEN_ENV`] carries
//! the bearer when set; an unset token fetches unauthenticated (the loopback
//! test-mock path — production answers 401 and the poll keeps last good).
//! Every failure shape (timeout, refused, non-2xx, oversized or malformed
//! body) is a dead-end [`OpsFetchError`]: the caller keeps the last good
//! record and the tick still succeeds. Errors and logs carry the failure
//! kind only — never the URL, the token, or override values.

use std::time::Duration;

use bytes::Bytes;
use http::{header::AUTHORIZATION, Request, Uri};
use http_body_util::{BodyExt, Empty, Limited};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};

use crate::settings::OpsRecord;

/// Full GET URL of the ops record. Operator-set Worker var, forwarded on an
/// explicit container line (never a flag: `_URL` names cannot join
/// `FORWARDED_FLAGS`). Unset, empty, or off-shape disables the layer.
pub const OPS_CONFIG_URL_ENV: &str = "TWO_OPS_CONFIG_URL";

/// Bearer the container sends on the ops fetch: the ops control token's
/// value, forwarded only while the fetch URL is set, on its own secret
/// line. Unset fetches unauthenticated.
pub const OPS_CONFIG_TOKEN_ENV: &str = "TWO_OPS_CONFIG_TOKEN";

/// Single-fetch deadline, far under the 15 s poll cadence so a wedged fetch
/// can never starve the guild poll behind it.
pub const OPS_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Response cap: PUT bodies are capped at 16 KiB and the GET envelope adds
/// the baseline; 64 KiB leaves headroom without letting a rogue endpoint
/// page the poller.
const MAX_OPS_BODY_BYTES: usize = 64 * 1024;

/// Why one ops-record fetch produced no record. Kind-only by design: no
/// URL, token, or body bytes, so callers can log it directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OpsFetchError {
    /// The fetch exceeded its deadline.
    #[error("ops fetch timed out")]
    Timeout,
    /// The request never completed: refused, unreachable, or an unsendable
    /// bearer. A bearer that cannot form a header, or targets plain HTTP
    /// outside explicit loopback, fails rather than downgrading to unauthenticated.
    #[error("ops fetch refused or unreachable")]
    Transport,
    /// The endpoint answered a non-success status (the code, for the log).
    #[error("ops endpoint returned status {0}")]
    Status(u16),
    /// The response exceeded `MAX_OPS_BODY_BYTES` (or the body stream
    /// broke mid-read, which surfaces through the same cap).
    #[error("ops response exceeded the body cap")]
    BodyTooLarge,
    /// The body was not JSON, or not a valid ops record.
    #[error("ops response was not a valid ops record")]
    Parse,
}

/// The configured fetch URL, or `None` when the layer is disabled (unset,
/// empty, unparseable, or a non-HTTP scheme). Read per tick so a URL change
/// takes effect without a restart.
#[must_use]
pub fn ops_url_from_env() -> Option<Uri> {
    let raw = std::env::var(OPS_CONFIG_URL_ENV).ok()?;
    parse_ops_url(&raw)
}

/// The configured fetch bearer, or `None` to fetch unauthenticated. Empty
/// counts as unset; anything else travels verbatim (tokens are exact — a
/// pasted space is a real mismatch, not trimmable).
#[must_use]
pub fn ops_token_from_env() -> Option<String> {
    parse_ops_token(std::env::var(OPS_CONFIG_TOKEN_ENV).ok())
}

fn parse_ops_token(token: Option<String>) -> Option<String> {
    token.filter(|value| !value.is_empty())
}

fn bearer_transport_allowed(url: &Uri) -> bool {
    match url.scheme_str() {
        Some("https") => true,
        Some("http") => url.authority().is_some_and(|authority| {
            crate::send_admission::is_loopback_http(&format!("http://{authority}"))
        }),
        _ => false,
    }
}

/// Parse + validate a fetch URL without touching the environment: HTTP(S)
/// absolute URIs only, surrounding whitespace tolerated (a pasted newline
/// must not silently disable the incident lever).
fn parse_ops_url(raw: &str) -> Option<Uri> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let uri: Uri = trimmed.parse().ok()?;
    match uri.scheme_str() {
        Some("http" | "https") => Some(uri),
        _ => None,
    }
}

/// GET the ops record at `url`, exactly as given, with an optional bearer.
/// The deadline covers the whole fetch — connect, headers, and body — so a
/// stalled body cannot starve the guild poll behind it.
///
/// A fresh client per call (feeds_connector precedent): the 15 s cadence
/// makes pool reuse worthless and keeps test-global clients out of the
/// picture. A bearer requires HTTPS or explicit loopback HTTP; other HTTP
/// targets fail before dialing. Tokenless HTTP remains available for fixtures.
/// Other schemes never reach here (`parse_ops_url` refuses them first).
pub async fn fetch_ops_record(
    url: &Uri,
    token: Option<&str>,
    timeout: Duration,
) -> Result<OpsRecord, OpsFetchError> {
    tokio::time::timeout(timeout, fetch_inner(url, token))
        .await
        .map_err(|_| OpsFetchError::Timeout)?
}

async fn fetch_inner(url: &Uri, token: Option<&str>) -> Result<OpsRecord, OpsFetchError> {
    if token.is_some() && !bearer_transport_allowed(url) {
        return Err(OpsFetchError::Transport);
    }
    let https = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .build();
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new())
        .retry_canceled_requests(false)
        .build(https);
    let mut request = Request::builder().method("GET").uri(url.clone());
    if let Some(token) = token {
        let bearer = format!("Bearer {token}");
        let value: http::header::HeaderValue =
            bearer.parse().map_err(|_| OpsFetchError::Transport)?;
        request = request.header(AUTHORIZATION, value);
    }
    let request = request
        .body(Empty::<Bytes>::new())
        .map_err(|_| OpsFetchError::Transport)?;
    let response = client
        .request(request)
        .await
        .map_err(|_| OpsFetchError::Transport)?;
    if !response.status().is_success() {
        return Err(OpsFetchError::Status(response.status().as_u16()));
    }
    let body = Limited::new(response.into_body(), MAX_OPS_BODY_BYTES)
        .collect()
        .await
        .map_err(|_| OpsFetchError::BodyTooLarge)?;
    let value: serde_json::Value =
        serde_json::from_slice(&body.to_bytes()).map_err(|_| OpsFetchError::Parse)?;
    OpsRecord::parse(&value).ok_or(OpsFetchError::Parse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[test]
    fn parse_ops_url_accepts_http_urls_verbatim() {
        for raw in [
            "http://127.0.0.1:9/ops/config",
            "https://ops.example.invalid/ops/config",
            "https://ops.example.invalid/ops/config?x=1",
            "  https://ops.example.invalid/ops/config\n",
        ] {
            let uri = parse_ops_url(raw).expect("http(s) parses");
            assert!(matches!(uri.scheme_str(), Some("http" | "https")), "{raw}");
        }
        // The query rides along untouched: the fetch adds nothing, and the
        // Worker answers 404 for a queried URL (a Status failure, kept good).
        let queried =
            parse_ops_url("https://ops.example.invalid/ops/config?x=1").expect("query ok");
        assert_eq!(queried.query(), Some("x=1"));
    }

    #[test]
    fn parse_ops_url_refuses_anything_not_plain_http() {
        for raw in [
            "",
            "   ",
            "not a url",
            "ftp://ops.example.invalid/ops/config",
            "file:///ops/config",
            "ops.example.invalid/ops/config",
            "/ops/config",
        ] {
            assert_eq!(parse_ops_url(raw), None, "{raw:?}");
        }
    }

    /// Loopback stand-in for the Worker's GET /ops/config, capturing the
    /// Authorization header it saw so tests pin the bearer contract.
    struct MockOps {
        seen_auth: Arc<Mutex<Vec<Option<String>>>>,
        _server: tokio::task::JoinHandle<()>,
        url: Uri,
    }

    impl MockOps {
        async fn serve(
            handler: impl FnOnce(Arc<Mutex<Vec<Option<String>>>>) -> axum::Router,
        ) -> Self {
            let seen_auth = Arc::new(Mutex::new(Vec::new()));
            let app = handler(Arc::clone(&seen_auth));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("mock ops listen");
            let url: Uri = format!(
                "http://{}/ops/config",
                listener.local_addr().expect("mock ops address")
            )
            .parse()
            .expect("mock url parses");
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.expect("mock ops serve");
            });
            Self {
                seen_auth,
                _server: server,
                url,
            }
        }

        fn auths(&self) -> Vec<Option<String>> {
            self.seen_auth.lock().expect("auth capture lock").clone()
        }
    }

    fn record_handler(seen: Arc<Mutex<Vec<Option<String>>>>) -> axum::Router {
        axum::Router::new().route(
            "/ops/config",
            axum::routing::get(move |request: axum::extract::Request| async move {
                seen.lock().expect("auth capture lock").push(
                    request
                        .headers()
                        .get(AUTHORIZATION)
                        .and_then(|v| v.to_str().ok().map(str::to_owned)),
                );
                // The request must arrive exactly as configured: this
                // path, no query, nothing appended.
                assert_eq!(request.uri().path(), "/ops/config");
                assert_eq!(request.uri().query(), None);
                axum::Json(json!({
                    "deploymentId": "mock-ops-deployment",
                    "epoch": 4,
                    "overrides": {"TWO_AUTOMOD_MENTION_LIMIT": "9"},
                    "baseline": {"TWO_AUTOMOD": "1"},
                    "enforcement": "pending",
                }))
                .into_response()
            }),
        )
    }

    #[tokio::test]
    async fn fetch_returns_the_record_and_sends_the_bearer() {
        let mock = MockOps::serve(record_handler).await;
        let record = fetch_ops_record(&mock.url, Some("test-token"), Duration::from_secs(5))
            .await
            .expect("fetch succeeds");
        assert_eq!(record.epoch, 4);
        assert_eq!(
            record
                .overrides
                .get("TWO_AUTOMOD_MENTION_LIMIT")
                .map(String::as_str),
            Some("9")
        );
        assert_eq!(mock.auths(), vec![Some("Bearer test-token".to_owned())]);
    }

    #[tokio::test]
    async fn fetch_without_token_sends_no_authorization_header() {
        let mock = MockOps::serve(record_handler).await;
        fetch_ops_record(&mock.url, None, Duration::from_secs(5))
            .await
            .expect("unauthenticated fetch succeeds");
        assert_eq!(mock.auths(), vec![None]);
    }

    #[tokio::test]
    async fn fetch_maps_failure_shapes_to_dead_end_errors() {
        // Non-2xx carries the status for the log.
        let failing = MockOps::serve(|_seen| {
            axum::Router::new().route(
                "/ops/config",
                axum::routing::get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
            )
        })
        .await;
        assert_eq!(
            fetch_ops_record(&failing.url, None, Duration::from_secs(5)).await,
            Err(OpsFetchError::Status(500))
        );
        // Malformed and off-shape bodies are Parse failures.
        for body in ["not-json{{{", r#"{"epoch": 1}"#] {
            let body = body.to_owned();
            let value = body.clone();
            let malformed = MockOps::serve(move |_seen| {
                axum::Router::new().route(
                    "/ops/config",
                    axum::routing::get(move || {
                        let body = value.clone();
                        async move { body }
                    }),
                )
            })
            .await;
            assert_eq!(
                fetch_ops_record(&malformed.url, None, Duration::from_secs(5)).await,
                Err(OpsFetchError::Parse),
                "{body:?}"
            );
        }
        // Oversized bodies hit the cap instead of paging the poller.
        let huge = MockOps::serve(|_seen| {
            axum::Router::new().route(
                "/ops/config",
                axum::routing::get(|| async { "x".repeat(MAX_OPS_BODY_BYTES + 1) }),
            )
        })
        .await;
        assert_eq!(
            fetch_ops_record(&huge.url, None, Duration::from_secs(5)).await,
            Err(OpsFetchError::BodyTooLarge)
        );
    }

    #[tokio::test]
    async fn fetch_refused_endpoint_is_a_transport_failure() {
        // Bind-then-drop hands back a port nothing answers on.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("probe listen");
        let url: Uri = format!(
            "http://{}/ops/config",
            listener.local_addr().expect("probe address")
        )
        .parse()
        .expect("probe url parses");
        drop(listener);
        assert_eq!(
            fetch_ops_record(&url, None, Duration::from_secs(5)).await,
            Err(OpsFetchError::Transport)
        );
    }

    #[tokio::test]
    async fn fetch_slow_endpoint_times_out() {
        let slow = MockOps::serve(|_seen| {
            axum::Router::new().route(
                "/ops/config",
                axum::routing::get(|| async {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    "too late"
                }),
            )
        })
        .await;
        assert_eq!(
            fetch_ops_record(&slow.url, None, Duration::from_millis(50)).await,
            Err(OpsFetchError::Timeout)
        );
    }

    #[tokio::test]
    async fn fetch_stalled_body_times_out() {
        // Headers flush at once; the body stalls past the deadline. The
        // timeout covers the whole fetch, not just connect + headers.
        let stalled = MockOps::serve(|_seen| {
            axum::Router::new().route(
                "/ops/config",
                axum::routing::get(|| async {
                    let body = futures_util::stream::once(async {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        Ok::<_, std::convert::Infallible>(Bytes::from_static(b"too late"))
                    });
                    axum::body::Body::from_stream(body)
                }),
            )
        })
        .await;
        assert_eq!(
            fetch_ops_record(&stalled.url, None, Duration::from_millis(50)).await,
            Err(OpsFetchError::Timeout)
        );
    }

    #[test]
    fn ops_token_parser_preserves_nonempty_tokens_verbatim() {
        assert_eq!(parse_ops_token(None), None);
        assert_eq!(parse_ops_token(Some(String::new())), None);
        assert_eq!(
            parse_ops_token(Some("fixture".to_owned())),
            Some("fixture".to_owned())
        );
        assert_eq!(
            parse_ops_token(Some(" fixture ".to_owned())),
            Some(" fixture ".to_owned())
        );
    }

    #[test]
    fn bearer_transport_requires_https_or_explicit_loopback_http() {
        for raw in [
            "https://ops.example.invalid/ops/config",
            "http://127.0.0.1:1234/ops/config",
            "http://127.0.0.2:1234/ops/config",
            "http://localhost:1234/ops/config",
            "http://[::1]:1234/ops/config",
        ] {
            assert!(
                bearer_transport_allowed(&raw.parse().expect("fixture URI")),
                "{raw}"
            );
        }
        for raw in [
            "http://ops.example.invalid/ops/config",
            "http://0.0.0.0:1234/ops/config",
            "http://192.0.2.1:1234/ops/config",
            "http://localhost.evil:1234/ops/config",
            "http://fixture@localhost:1234/ops/config",
            "ftp://127.0.0.1:1234/ops/config",
            "/ops/config",
        ] {
            assert!(
                !bearer_transport_allowed(&raw.parse().expect("fixture URI")),
                "{raw}"
            );
        }
    }

    #[tokio::test]
    async fn fetch_non_loopback_bearer_fails_closed_before_sending() {
        let mock = MockOps::serve(record_handler).await;
        // Linux can dial an unspecified IPv4 address locally, but it is not
        // an explicit loopback target. The guard must reject before dialing.
        let url: Uri = mock
            .url
            .to_string()
            .replace("127.0.0.1", "0.0.0.0")
            .parse()
            .expect("non-loopback fixture URI");
        assert_eq!(
            fetch_ops_record(&url, Some("test-token"), Duration::from_secs(5)).await,
            Err(OpsFetchError::Transport)
        );
        assert!(mock.auths().is_empty());
        // Tokenless HTTP behavior remains unchanged.
        assert_eq!(
            fetch_ops_record(&url, None, Duration::from_secs(5))
                .await
                .expect("tokenless local fixture fetch")
                .epoch,
            4
        );
        assert_eq!(mock.auths(), vec![None]);
    }

    #[tokio::test]
    async fn fetch_unsendable_bearer_fails_closed() {
        let mock = MockOps::serve(record_handler).await;
        // A control character cannot form a header value: the fetch fails
        // rather than downgrading to unauthenticated.
        assert_eq!(
            fetch_ops_record(&mock.url, Some("bad\ntoken"), Duration::from_secs(5)).await,
            Err(OpsFetchError::Transport)
        );
        assert!(mock.auths().is_empty());
    }
}
