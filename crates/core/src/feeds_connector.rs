//! Pinned public-address HTTPS connector for feed fetching.
//!
//! [`fetch_feed`] is the production entry point: it validates the source,
//! resolves **all** A/AAAA answers through a [`FeedResolver`], pins them into a
//! [`PublicRequest`], and drives a [`FeedConnector`] that may dial only those
//! addresses. Every redirect hop re-runs the same validation, so a record that
//! flips between checks is either re-validated (new lookup, new pin) or
//! unreachable — the socket layer never resolves a name itself, never uses a
//! proxy, and never reuses a pooled connection: a fresh [`Client`] is built per
//! request and dropped with it.
//!
//! The stock connector is [`PinnedHttpsConnector`]: hyper 1.x over rustls
//! (ring + Mozilla roots — the same stack [`crate::backup::http`] uses) with an
//! [`HttpConnector`] whose resolver is [`PinnedResolver`]. `PinnedResolver`
//! answers only the request's pinned socket addresses and refuses any other
//! name, so the only dial targets are the ones [`PublicRequest::prepare`]
//! validated. TLS SNI and certificate verification still come from the URL
//! hostname via hyper-rustls' `DefaultServerNameResolver`; pinning addresses
//! never replaces identity.
//!
//! Redirects are manual: the legacy client never auto-follows, each hop is
//! re-planned by [`redirect_target`] (same host, HTTPS only, bounded by
//! [`MAX_REDIRECT_HOPS`]), and the previous response stream is dropped before
//! the next dial. DNS names resolve from the parsed `Url::host()` so IPv6
//! literals reach the resolver bracket-free; TLS identity still comes from
//! the untouched URL. Response bodies are bounded twice — on the compressed
//! wire and again decompressed into [`LimitedBody`] — before the caller ever
//! sees parsed XML. Concatenated gzip members all decode; stacked
//! `Content-Encoding` layers are refused; non-ASCII policy headers
//! (content-type, content-encoding, location) fail closed while unrelated
//! headers stay lenient. The absolute deadline also governs the blocking
//! decode and is re-checked before success is returned.
//!
//! Tests inject [`FeedResolver`]/[`FeedConnector`] doubles; there is no seam
//! that lets a caller dial an address `PublicRequest` did not approve.

#[cfg(test)]
#[path = "feeds_connector_tests.rs"]
mod tests;

use std::future::Future;
use std::io::{self, Read};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http::header::{ACCEPT, ACCEPT_ENCODING, USER_AGENT};
use http::{Method, Request, StatusCode};
use http_body_util::{BodyStream, Empty};
use hyper::body::Incoming;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use thiserror::Error;
use tower_service::Service;
use url::{Host, Url};

use crate::feeds_http::{
    redirect_target, validate_content_type, validate_source, FetchError, LimitedBody,
    PublicRequest, FEED_READ_TIMEOUT_MS, FEED_USER_AGENT, MAX_FEED_BYTES,
};

/// Redirect statuses the legacy fetcher honoured (`Location` re-planned, never
/// auto-followed).
const REDIRECT_STATUSES: [StatusCode; 5] = [
    StatusCode::MOVED_PERMANENTLY,
    StatusCode::FOUND,
    StatusCode::SEE_OTHER,
    StatusCode::TEMPORARY_REDIRECT,
    StatusCode::PERMANENT_REDIRECT,
];

/// Encodings we advertise and can decode. Anything else is refused rather than
/// passed half-parsed to the XML layer.
const ACCEPT_ENCODING_VALUE: &str = "gzip, deflate";

/// Errors the connector can raise on top of the pure [`FetchError`] policy.
#[derive(Debug, Error)]
pub enum FeedConnectError {
    /// A policy refusal from [`crate::feeds_http`]: bad source, non-public
    /// address, redirect refused, body too large, bad encoding or content type.
    #[error(transparent)]
    Policy(#[from] FetchError),
    /// The injected/system resolver failed for the feed host.
    #[error("feed DNS lookup failed: {0}")]
    Resolve(#[source] io::Error),
    /// TLS, TCP or HTTP-layer failure, or a corrupt compressed body.
    #[error("feed request failed: {0}")]
    Transport(String),
    /// The whole fetch — DNS, every hop, the body read — exceeded the deadline.
    #[error("feed fetch exceeded the {0} ms total deadline")]
    Deadline(u64),
    /// Server answered an encoding we did not advertise (e.g. `br`).
    #[error("feed body uses unsupported content encoding {0:?}")]
    UnsupportedEncoding(String),
}

/// One decompressed-or-raw response body chunk, or a transport error that
/// aborts the read. Production bodies come from hyper; tests inject their own.
pub type FeedBodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, FeedConnectError>> + Send>>;

/// A connector response: final status, wire headers and the byte stream.
/// Headers are stored name-lowercased so [`FeedResponse::header`] can match
/// case-insensitively without `HeaderMap` in the test seam.
pub struct FeedResponse {
    /// HTTP status of this hop.
    pub status: StatusCode,
    /// `(lowercased-name, value)` pairs exactly as received.
    pub headers: Vec<(String, String)>,
    /// Response body stream; consumed and dropped exactly once.
    pub body: FeedBodyStream,
}

impl FeedResponse {
    /// First header value for `name`, case-insensitive.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A fetched feed body after redirect chasing, decompression and UTF-8 checks.
#[derive(Debug)]
pub struct FetchedFeed {
    /// URL of the final hop (differs from the source after redirects).
    pub url: Url,
    /// Final status — the connector does not error on 4xx/5xx; callers decide.
    pub status: StatusCode,
    /// Decompressed UTF-8 body, at most [`MAX_FEED_BYTES`] before parsing.
    pub body: String,
}

/// Tunables for [`fetch_feed_with`]. The deadline is a single wall-clock
/// budget covering DNS, every redirect hop and the body read — production
/// callers keep the default so `FEED_READ_TIMEOUT_MS` stays authoritative.
#[derive(Debug, Clone, Copy)]
pub struct FetchOptions {
    /// Total fetch budget; defaults to [`FEED_READ_TIMEOUT_MS`].
    pub deadline: Duration,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            deadline: Duration::from_millis(FEED_READ_TIMEOUT_MS),
        }
    }
}

/// DNS seam: resolve `host` to every A/AAAA answer for `port`. All answers are
/// validated by [`PublicRequest::prepare`]; implementations must not filter.
pub trait FeedResolver {
    /// Resolve one host; returns every answer or an `io::Error`.
    fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> impl Future<Output = Result<Vec<IpAddr>, io::Error>> + Send + '_;
}

/// Transport seam: perform exactly one HTTPS GET against `request`, dialling
/// only `request.addresses()` and keeping `request.url()`'s host for the
/// `Host` header, TLS SNI and certificate verification. Implementations must
/// not follow redirects, reuse pooled connections, consult proxies or issue a
/// second DNS lookup.
pub trait FeedConnector {
    /// Issue the GET; returns the raw response for the caller to drain.
    fn get<'a>(
        &'a self,
        request: &'a PublicRequest,
    ) -> impl Future<Output = Result<FeedResponse, FeedConnectError>> + Send + 'a;
}

/// System DNS via `tokio::net::lookup_host` (getaddrinfo). The connector's
/// production resolver; tests inject a scripted [`FeedResolver`] instead.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioResolver;

impl FeedResolver for TokioResolver {
    fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> impl Future<Output = Result<Vec<IpAddr>, io::Error>> + Send + '_ {
        let target = (host.to_owned(), port);
        async move {
            let addrs = tokio::net::lookup_host(target).await?;
            Ok(addrs.map(|addr| addr.ip()).collect())
        }
    }
}

/// Fetch `source` with the production resolver and pinned HTTPS transport —
/// the only entry point a poller should use. Identical to
/// [`fetch_feed_with`] with [`TokioResolver`], [`PinnedHttpsConnector`] and
/// [`FetchOptions::default`].
pub async fn fetch_feed(source: &str) -> Result<FetchedFeed, FeedConnectError> {
    fetch_feed_with(
        &TokioResolver,
        &PinnedHttpsConnector,
        source,
        &FetchOptions::default(),
    )
    .await
}

/// Absolute fetch deadline, derived once from [`FetchOptions`]. The outer
/// `tokio::time::timeout` preempts pending DNS/body awaits, but synchronous
/// decode cannot be polled — so the same instant is threaded into the body
/// path and checked before success is returned.
#[derive(Debug, Clone, Copy)]
struct FetchDeadline {
    at: Instant,
    budget_ms: u64,
}

impl FetchDeadline {
    fn new(deadline: Duration) -> Self {
        Self {
            at: Instant::now() + deadline,
            budget_ms: deadline.as_millis() as u64,
        }
    }

    fn check(&self) -> Result<(), FeedConnectError> {
        if Instant::now() >= self.at {
            return Err(FeedConnectError::Deadline(self.budget_ms));
        }
        Ok(())
    }
}

/// Full fetch pipeline under one deadline: validate → resolve all answers →
/// pin → GET → manual redirect loop → content-type → bounded compressed read
/// → bounded decompress into [`LimitedBody`] → UTF-8.
pub async fn fetch_feed_with<R, C>(
    resolver: &R,
    connector: &C,
    source: &str,
    options: &FetchOptions,
) -> Result<FetchedFeed, FeedConnectError>
where
    R: FeedResolver,
    C: FeedConnector,
{
    let deadline = FetchDeadline::new(options.deadline);
    tokio::time::timeout(
        options.deadline,
        fetch_inner(resolver, connector, source, &deadline),
    )
    .await
    .map_err(|_| FeedConnectError::Deadline(options.deadline.as_millis() as u64))?
}

async fn fetch_inner<R: FeedResolver, C: FeedConnector>(
    resolver: &R,
    connector: &C,
    source: &str,
    deadline: &FetchDeadline,
) -> Result<FetchedFeed, FeedConnectError> {
    let mut request = plan_request(resolver, validate_source(source)?).await?;
    let mut hops = 0usize;
    loop {
        let response = connector.get(&request).await?;
        let status = response.status;
        if REDIRECT_STATUSES.contains(&status) {
            // Drop the hop body before planning the next request: the old
            // connection is cancelled, never drained into the pool.
            // A non-ASCII Location is a refused redirect, never a joined one.
            let location = response
                .policy_header("location", FetchError::RedirectRefused)?
                .unwrap_or("")
                .to_owned();
            let target = redirect_target(request.url(), &location, hops)?;
            request = plan_request(resolver, target).await?;
            hops += 1;
            continue;
        }
        return Ok(FetchedFeed {
            url: request.url().clone(),
            status,
            body: read_body(response, deadline).await?,
        });
    }
}

/// Resolve + pin one hop: every answer flows through [`PublicRequest::prepare`]
/// so literal hosts must resolve to themselves and any private/mixed answer
/// refuses the hop outright.
async fn plan_request<R: FeedResolver>(
    resolver: &R,
    url: Url,
) -> Result<PublicRequest, FeedConnectError> {
    // Resolve from the parsed `Url::host()`, never `host_str()`: the latter
    // retains IPv6 brackets (`[2001:db8::1]`) which `lookup_host` rejects,
    // while the parsed form is bracket-free. TLS identity still comes from
    // the untouched URL.
    let host = match url.host() {
        Some(Host::Domain(domain)) => domain.to_owned(),
        Some(Host::Ipv4(ip)) => ip.to_string(),
        Some(Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(FetchError::InvalidSource.into()),
    };
    let port = url
        .port_or_known_default()
        .ok_or(FetchError::InvalidSource)?;
    let resolved = resolver
        .resolve(&host, port)
        .await
        .map_err(FeedConnectError::Resolve)?;
    Ok(PublicRequest::prepare(url, &resolved)?)
}

/// Content-type check first, then compressed-then-decompressed bounds.
///
/// The decoded coding list is validated before the wire body is drained, so a
/// hop declaring an unsupported coding fails without consuming the stream.
async fn read_body(
    response: FeedResponse,
    deadline: &FetchDeadline,
) -> Result<String, FeedConnectError> {
    validate_content_type(
        response.policy_header("content-type", FetchError::UnsupportedContentType)?,
    )?;
    // A malformed declared length grants nothing: ignore it and let the
    // cumulative streaming cap stay authoritative.
    let advertised = content_length(response.header("content-length").filter(|v| v.is_ascii()));
    let coding = content_codings(&response)?;
    let mut limited = LimitedBody::new(advertised)?;
    let wire = read_wire(response.body).await?;
    let decoded = decode_later(coding, wire, deadline).await?;
    for chunk in decoded.chunks(8192) {
        deadline.check()?;
        limited.push(chunk)?;
    }
    deadline.check()?;
    Ok(limited.finish()?)
}

fn content_length(header: Option<&str>) -> Option<u64> {
    header.and_then(|value| value.trim().parse::<u64>().ok())
}

/// Read the compressed wire bytes with the same ceiling the policy applies to
/// bodies: feeds are small, and an oversized wire can only precede an
/// oversized (or hostile) decompressed body.
async fn read_wire(mut stream: FeedBodyStream) -> Result<Vec<u8>, FeedConnectError> {
    let mut wire = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if chunk.len() > MAX_FEED_BYTES - wire.len() {
            return Err(FetchError::TooLarge.into());
        }
        wire.extend_from_slice(&chunk);
    }
    Ok(wire)
}

/// Headers that drive policy: content-type, content-encoding, redirect
/// location. Non-ASCII bytes in these are a policy refusal — silently mapping
/// them to empty would turn a declared `br` into `identity`, drop the
/// content-type gate, or join a garbage redirect. The caller picks the
/// matching [`FetchError`] so a bad Location reads as a refused redirect,
/// not a bad body. All other headers stay lenient.
impl FeedResponse {
    fn policy_header(
        &self,
        name: &str,
        refusal: FetchError,
    ) -> Result<Option<&str>, FeedConnectError> {
        self.header(name)
            .map(|value| {
                if value.is_ascii() {
                    Ok(value)
                } else {
                    Err(FeedConnectError::Policy(refusal))
                }
            })
            .transpose()
    }
}

/// Coding declared on the wire, validated before any body byte is drained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireCoding {
    Identity,
    Gzip,
    Deflate,
}

/// Parse **every** `Content-Encoding` header value in declared order and
/// return the single validated coding. Stacked codings are refused: the
/// decoder handles one layer, so a second layer would reach the XML layer
/// half-decoded. Comma-separated lists are split so `Content-Encoding: gzip,
/// br` cannot hide an unadvertised layer. Repeated `identity` (or empty token
/// runs) collapse to identity; any other repeat is a stack and is refused.
fn content_codings(response: &FeedResponse) -> Result<WireCoding, FeedConnectError> {
    let mut coding: Option<WireCoding> = None;
    for (_, value) in response
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-encoding"))
    {
        let value = value
            .is_ascii()
            .then_some(value)
            .ok_or(FeedConnectError::Policy(FetchError::UnsupportedContentType))?;
        for layer in value.split(',') {
            let layer = layer.trim().to_ascii_lowercase();
            if layer.is_empty() {
                continue;
            }
            let next = match layer.as_str() {
                "identity" => WireCoding::Identity,
                "gzip" | "x-gzip" => WireCoding::Gzip,
                "deflate" => WireCoding::Deflate,
                other => return Err(FeedConnectError::UnsupportedEncoding(other.to_owned())),
            };
            match (coding, next) {
                (None, next) => coding = Some(next),
                (Some(WireCoding::Identity), WireCoding::Identity) => {}
                _ => {
                    return Err(FeedConnectError::UnsupportedEncoding("stacked".to_owned()));
                }
            }
        }
    }
    Ok(coding.unwrap_or(WireCoding::Identity))
}

/// Run the single validated decode off the async worker (`spawn_blocking`)
/// under the absolute deadline: a slow `GzDecoder` cannot silently overrun
/// the total budget the wrapper promises.
async fn decode_later(
    coding: WireCoding,
    wire: Vec<u8>,
    deadline: &FetchDeadline,
) -> Result<Vec<u8>, FeedConnectError> {
    deadline.check()?;
    let at = deadline.at;
    let budget_ms = deadline.budget_ms;
    let decoded = tokio::time::timeout_at(
        at.into(),
        tokio::task::spawn_blocking(move || decode_wire(&wire, coding)),
    )
    .await
    .map_err(|_| FeedConnectError::Deadline(budget_ms))?
    .map_err(|err| FeedConnectError::Transport(format!("feed body decode failed: {err}")))??;
    // A ready task can win the timeout poll even after the timer expires.
    deadline.check()?;
    Ok(decoded)
}

/// Decode `Content-Encoding` bodies; decompressed output is capped at
/// `MAX_FEED_BYTES` regardless of declared sizes, so a zip-bomb dies in the
/// stream, not in the parser.
fn decode_wire(wire: &[u8], coding: WireCoding) -> Result<Vec<u8>, FeedConnectError> {
    let decoded = match coding {
        WireCoding::Identity => Ok(wire.to_vec()),
        // `MultiGzDecoder` consumes concatenated gzip members: `GzDecoder`
        // stops at the first member boundary and silently truncates the rest.
        WireCoding::Gzip => bounded_decode(flate2::read::MultiGzDecoder::new(wire)),
        // `deflate` is ambiguous on the wire: try the zlib wrapper first,
        // then raw deflate (what undici tolerates). A decompressed overflow
        // is never retried — TooLarge is final.
        WireCoding::Deflate => {
            bounded_decode(flate2::read::ZlibDecoder::new(wire)).or_else(|err| match err {
                DecodeError::TooLarge => Err(DecodeError::TooLarge),
                DecodeError::Io(_) => bounded_decode(flate2::read::DeflateDecoder::new(wire)),
            })
        }
    };
    decoded.map_err(|err| match err {
        DecodeError::TooLarge => FetchError::TooLarge.into(),
        DecodeError::Io(err) => {
            FeedConnectError::Transport(format!("feed body decode failed: {err}"))
        }
    })
}

enum DecodeError {
    Io(io::Error),
    TooLarge,
}

fn bounded_decode<R: Read>(mut decoder: R) -> Result<Vec<u8>, DecodeError> {
    let mut limited = decoder.by_ref().take(MAX_FEED_BYTES as u64 + 1);
    let mut out = Vec::new();
    limited.read_to_end(&mut out).map_err(DecodeError::Io)?;
    if out.len() > MAX_FEED_BYTES {
        return Err(DecodeError::TooLarge);
    }
    Ok(out)
}

/// Answers [`HttpConnector`]'s `Service<Name>` resolution for one feed
/// request: the pinned socket addresses for this request's host, and an error
/// for anything else. Fields stay private — constructed only from a validated
/// [`PublicRequest`] — so no caller can inject unvalidated addresses.
#[derive(Debug, Clone)]
struct PinnedResolver {
    host: String,
    addrs: Arc<[SocketAddr]>,
}

impl PinnedResolver {
    fn new(request: &PublicRequest) -> Self {
        Self {
            host: normalized_host(request.url().host_str().unwrap_or("")),
            addrs: request.addresses().into(),
        }
    }

    /// Test-only constructor: loopback fixtures cannot come from
    /// `PublicRequest` (it refuses non-public addresses), so the only callers
    /// are `#[cfg(test)]`.
    #[cfg(test)]
    fn for_test(host: &str, addrs: &[SocketAddr]) -> Self {
        Self {
            host: normalized_host(host),
            addrs: addrs.into(),
        }
    }
}

fn normalized_host(host: &str) -> String {
    host.trim_end_matches('.')
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

/// Owning iterator over one request's pinned addresses. Handing out the `Arc`
/// bumps a refcount instead of copying the address list for every dial
/// attempt, and the iterator stays `Send` so hyper can move it across threads.
#[derive(Debug)]
struct PinnedAddrs {
    addrs: Arc<[SocketAddr]>,
    next: usize,
}

impl Iterator for PinnedAddrs {
    type Item = SocketAddr;

    fn next(&mut self) -> Option<SocketAddr> {
        let addr = self.addrs.get(self.next).copied()?;
        self.next += 1;
        Some(addr)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.addrs.len().saturating_sub(self.next);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for PinnedAddrs {}

impl Service<Name> for PinnedResolver {
    type Response = PinnedAddrs;
    type Error = io::Error;
    type Future = std::future::Ready<Result<Self::Response, io::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        if normalized_host(name.as_str()) == self.host {
            std::future::ready(Ok(PinnedAddrs {
                addrs: Arc::clone(&self.addrs),
                next: 0,
            }))
        } else {
            // The dialer must never resolve a name the request did not pin;
            // refusal fails the connection before any socket opens.
            std::future::ready(Err(io::Error::other(format!(
                "pinned feed resolver refused unexpected name {name:?}"
            ))))
        }
    }
}

/// Production transport: one hyper request, pinned dial addresses, HTTPS only.
#[derive(Debug, Clone, Copy, Default)]
pub struct PinnedHttpsConnector;

impl PinnedHttpsConnector {
    /// Build the request the stock connector sends. Extracted so tests can
    /// prove the wire headers without standing up TLS.
    fn build_request(url: &Url) -> Result<Request<Empty<Bytes>>, FeedConnectError> {
        Request::builder()
            .method(Method::GET)
            .uri(url.as_str())
            .header(USER_AGENT, FEED_USER_AGENT)
            .header(ACCEPT_ENCODING, ACCEPT_ENCODING_VALUE)
            .header(ACCEPT, "*/*")
            .body(Empty::new())
            .map_err(|err| FeedConnectError::Transport(format!("feed request build failed: {err}")))
    }
}

impl FeedConnector for PinnedHttpsConnector {
    async fn get<'a>(
        &'a self,
        request: &'a PublicRequest,
    ) -> Result<FeedResponse, FeedConnectError> {
        // The resolver answers only this request's pinned addresses, so
        // happy-eyeballs races and retries can only ever touch them.
        let mut http = HttpConnector::new_with_resolver(PinnedResolver::new(request));
        // The Uri still carries `https`; without this the connector
        // refuses the scheme before dialling.
        http.enforce_http(false);
        http.set_nodelay(true);
        let https = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .wrap_connector(http);
        // A fresh Client per request: the pool is empty at build and
        // dropped at drop, so no connection survives across requests or
        // across differently-pinned hosts.
        let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(https);
        let req = Self::build_request(request.url())?;
        let res = client
            .request(req)
            .await
            .map_err(|err| FeedConnectError::Transport(err.to_string()))?;
        Ok(FeedResponse::from_hyper(res))
    }
}

impl FeedResponse {
    fn from_hyper(res: hyper::Response<Incoming>) -> Self {
        let status = res.status();
        let headers = res
            .headers()
            .iter()
            .map(|(name, value)| {
                // Lossy, never silent: `from_utf8_lossy` emits U+FFFD for
                // non-ASCII bytes, so `policy_header` still sees and refuses
                // malformed policy metadata instead of reading it as empty.
                // Unrelated opaque headers keep their lossy value untouched.
                (
                    name.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        // Skip trailers; propagate stream errors so truncation fails loudly.
        let stream = BodyStream::new(res.into_body()).filter_map(|frame| async move {
            match frame {
                Ok(frame) => frame.into_data().ok().map(Ok),
                Err(err) => Some(Err(FeedConnectError::Transport(format!(
                    "feed body stream failed: {err}"
                )))),
            }
        });
        Self {
            status,
            headers,
            body: Box::pin(stream),
        }
    }
}
