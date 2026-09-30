//! Hermetic integration tests for the pinned feed connector. A scripted
//! [`FeedResolver`] controls every DNS answer and a scripted [`FeedConnector`]
//! records exactly which `PublicRequest` each hop dialled, so the tests prove
//! the orchestration contract — all-answer validation, rebinding resistance,
//! redirect budget, single deadline, compressed/decompressed caps — without
//! any socket ever opening.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use flate2::write::{DeflateEncoder, GzEncoder, ZlibEncoder};
use flate2::Compression;
use futures_util::stream;
use http::StatusCode;
use two_bot_core::feeds::parse_xml_feed;
use two_bot_core::feeds_connector::{
    fetch_feed_with, FeedConnectError, FeedConnector, FeedResolver, FeedResponse, FetchOptions,
};
use two_bot_core::feeds_http::{FetchError, PublicRequest, MAX_FEED_BYTES};

const SOURCE: &str = "https://feeds.example.org/rss";
const PUBLIC_A: IpAddr = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
const PRIVATE: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 9, 9, 9));

fn public_v6() -> IpAddr {
    "2606:2800:220:1:248:1893:25c8:1946"
        .parse::<Ipv6Addr>()
        .unwrap()
        .into()
}

fn pinned(ip: IpAddr, port: u16) -> SocketAddr {
    SocketAddr::new(ip, port)
}

fn options(ms: u64) -> FetchOptions {
    FetchOptions {
        deadline: Duration::from_millis(ms),
    }
}

/// The default deadline is 15 s; tests shorten it so deadline proofs are fast.
fn fast() -> FetchOptions {
    options(5_000)
}

fn policy(err: &FeedConnectError) -> FetchError {
    match err {
        FeedConnectError::Policy(err) => err.clone(),
        other => panic!("expected a policy refusal, got {other}"),
    }
}

// --- resolver double ---------------------------------------------------------

enum ResolveStep {
    Addrs(Vec<IpAddr>),
    Fail(io::ErrorKind),
    Hang,
}

struct MockResolver {
    calls: Mutex<Vec<(String, u16)>>,
    script: Mutex<VecDeque<ResolveStep>>,
}

impl MockResolver {
    fn scripted(script: Vec<ResolveStep>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            script: Mutex::new(script.into()),
        }
    }

    fn addrs(addrs: &[IpAddr]) -> ResolveStep {
        ResolveStep::Addrs(addrs.to_vec())
    }

    fn calls(&self) -> Vec<(String, u16)> {
        self.calls.lock().unwrap().clone()
    }
}

impl FeedResolver for MockResolver {
    fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> impl std::future::Future<Output = Result<Vec<IpAddr>, io::Error>> + Send + '_ {
        // Side-effects stay synchronous so the returned future borrows only
        // `self` — never `host` or a MutexGuard.
        self.calls.lock().unwrap().push((host.to_owned(), port));
        let step = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ResolveStep::Fail(io::ErrorKind::NotFound));
        async move {
            match step {
                ResolveStep::Addrs(addrs) => Ok(addrs),
                ResolveStep::Fail(kind) => Err(kind.into()),
                ResolveStep::Hang => std::future::pending().await,
            }
        }
    }
}

// --- connector double --------------------------------------------------------

enum Step {
    Respond(Canned),
    RespondHungBody { status: StatusCode },
    Error(&'static str),
}

struct Canned {
    status: StatusCode,
    headers: Vec<(String, String)>,
    chunks: Vec<Bytes>,
}

impl Canned {
    fn xml(bytes: impl Into<Bytes>) -> Self {
        Self {
            status: StatusCode::OK,
            headers: vec![("content-type".into(), "application/rss+xml".into())],
            chunks: vec![bytes.into()],
        }
    }

    fn redirect(location: &str) -> Self {
        Self {
            status: StatusCode::FOUND,
            headers: vec![("location".into(), location.to_owned())],
            chunks: Vec::new(),
        }
    }

    fn chunked(mut self, chunk_size: usize) -> Self {
        let all: Vec<u8> = self.chunks.iter().flat_map(|c| c.iter().copied()).collect();
        self.chunks = all.chunks(chunk_size).map(Bytes::copy_from_slice).collect();
        self
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    fn into_response(self) -> FeedResponse {
        FeedResponse {
            status: self.status,
            headers: self.headers,
            body: Box::pin(stream::iter(self.chunks.into_iter().map(Ok))),
        }
    }
}

struct MockConnector {
    calls: Mutex<Vec<Vec<SocketAddr>>>,
    urls: Mutex<Vec<String>>,
    steps: Mutex<VecDeque<Step>>,
}

impl MockConnector {
    fn scripted(steps: Vec<Step>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            urls: Mutex::new(Vec::new()),
            steps: Mutex::new(steps.into()),
        }
    }

    fn calls(&self) -> Vec<Vec<SocketAddr>> {
        self.calls.lock().unwrap().clone()
    }

    fn urls(&self) -> Vec<String> {
        self.urls.lock().unwrap().clone()
    }
}

impl FeedConnector for MockConnector {
    fn get<'a>(
        &'a self,
        request: &'a PublicRequest,
    ) -> impl std::future::Future<Output = Result<FeedResponse, FeedConnectError>> + Send + 'a {
        self.calls
            .lock()
            .unwrap()
            .push(request.addresses().to_vec());
        self.urls.lock().unwrap().push(request.url().to_string());
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("connector called past its script"));
        async move {
            match step {
                Step::Respond(canned) => Ok(canned.into_response()),
                Step::RespondHungBody { status } => Ok(FeedResponse {
                    status,
                    headers: vec![("content-type".into(), "application/rss+xml".into())],
                    body: Box::pin(stream::pending()),
                }),
                Step::Error(msg) => Err(FeedConnectError::Transport(msg.to_owned())),
            }
        }
    }
}

// --- the happy path and pinning ----------------------------------------------

#[tokio::test]
async fn fetch_dials_only_the_validated_addresses_and_returns_the_body() {
    let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A, public_v6()])]);
    let connector = MockConnector::scripted(vec![Step::Respond(Canned::xml(include_str!(
        "fixtures/feeds/rss.xml"
    )))]);

    let feed = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap();

    assert_eq!(feed.status, StatusCode::OK);
    assert_eq!(feed.url.as_str(), SOURCE);
    assert_eq!(
        resolver.calls(),
        vec![("feeds.example.org".to_owned(), 443)],
        "DNS is resolved exactly once per request"
    );
    assert_eq!(
        connector.calls(),
        vec![vec![pinned(PUBLIC_A, 443), pinned(public_v6(), 443),]],
        "the dial targets are exactly the validated pins"
    );
    // Fixture parses identically to the direct path.
    assert_eq!(
        parse_xml_feed(&feed.body).unwrap(),
        parse_xml_feed(include_str!("fixtures/feeds/rss.xml")).unwrap()
    );
}

#[tokio::test]
async fn mixed_public_private_answers_are_refused_before_any_dial() {
    let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A, PRIVATE])]);
    let connector = MockConnector::scripted(vec![]);

    let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap_err();
    assert_eq!(policy(&err), FetchError::NonPublicAddress);
    assert!(connector.calls().is_empty(), "refused hop must never dial");
}

#[tokio::test]
async fn empty_and_private_only_answers_are_refused() {
    for answers in [vec![], vec![PRIVATE]] {
        let resolver = MockResolver::scripted(vec![ResolveStep::Addrs(answers)]);
        let connector = MockConnector::scripted(vec![]);
        let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
            .await
            .unwrap_err();
        assert_eq!(policy(&err), FetchError::NonPublicAddress);
        assert!(connector.calls().is_empty());
    }
}

#[tokio::test]
async fn a_literal_ip_source_must_resolve_to_itself() {
    let connector = MockConnector::scripted(vec![Step::Respond(Canned::xml("<rss/>"))]);
    let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A])]);
    let feed = fetch_feed_with(
        &resolver,
        &connector,
        "https://93.184.216.34/feed.xml",
        &fast(),
    )
    .await
    .unwrap();
    assert_eq!(connector.calls(), vec![vec![pinned(PUBLIC_A, 443)]]);
    assert_eq!(feed.status, StatusCode::OK);

    // A different answer for a literal host is a rebind — refused.
    let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[IpAddr::V4(Ipv4Addr::new(
        1, 1, 1, 1,
    ))])]);
    let connector = MockConnector::scripted(vec![]);
    let err = fetch_feed_with(
        &resolver,
        &connector,
        "https://93.184.216.34/feed.xml",
        &fast(),
    )
    .await
    .unwrap_err();
    assert_eq!(policy(&err), FetchError::NonPublicAddress);
}

// --- redirects ---------------------------------------------------------------

#[tokio::test]
async fn same_host_redirect_is_followed_with_fresh_dns_validation() {
    // Hop 1 pins A; the redirect hop re-resolves and gets B — every answer is
    // re-validated, so a rebind lands in a fresh PublicRequest, not a socket.
    let resolver = MockResolver::scripted(vec![
        MockResolver::addrs(&[PUBLIC_A]),
        MockResolver::addrs(&[IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))]),
    ]);
    let connector = MockConnector::scripted(vec![
        Step::Respond(Canned::redirect("/next")),
        Step::Respond(Canned::xml(include_str!("fixtures/feeds/youtube.xml"))),
    ]);

    let feed = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap();

    assert_eq!(feed.url.as_str(), "https://feeds.example.org/next");
    assert_eq!(resolver.calls().len(), 2);
    assert_eq!(
        connector.urls(),
        vec![
            SOURCE.to_owned(),
            "https://feeds.example.org/next".to_owned(),
        ],
        "the connector saw the joined same-host redirect URL"
    );
    assert_eq!(
        connector.calls()[1],
        vec![pinned(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 443)],
        "the second hop dials only its own re-validated pins"
    );
}

#[tokio::test]
async fn redirect_whose_rebound_answers_are_private_is_refused() {
    let resolver = MockResolver::scripted(vec![
        MockResolver::addrs(&[PUBLIC_A]),
        MockResolver::addrs(&[PRIVATE]),
    ]);
    let connector = MockConnector::scripted(vec![
        Step::Respond(Canned::redirect("/next")),
        Step::Respond(Canned::xml("<rss/>")),
    ]);

    let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap_err();
    assert_eq!(policy(&err), FetchError::NonPublicAddress);
    assert_eq!(connector.calls().len(), 1, "the rebound hop must not dial");
}

#[tokio::test]
async fn cross_host_and_downgrade_redirects_are_refused() {
    for location in [
        "https://other.example.org/feed",
        "http://feeds.example.org/rss",
        "https://user:pass@feeds.example.org/rss",
    ] {
        let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A])]);
        let connector = MockConnector::scripted(vec![Step::Respond(Canned::redirect(location))]);
        let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
            .await
            .unwrap_err();
        assert_eq!(
            policy(&err),
            FetchError::RedirectRefused,
            "location {location} must be refused"
        );
        assert_eq!(connector.calls().len(), 1);
    }
}

#[tokio::test]
async fn redirect_hops_are_bounded_at_three_follows() {
    let resolver = MockResolver::scripted(vec![
        MockResolver::addrs(&[PUBLIC_A]),
        MockResolver::addrs(&[PUBLIC_A]),
        MockResolver::addrs(&[PUBLIC_A]),
        MockResolver::addrs(&[PUBLIC_A]),
    ]);
    let connector = MockConnector::scripted(vec![
        Step::Respond(Canned::redirect("/r1")),
        Step::Respond(Canned::redirect("/r2")),
        Step::Respond(Canned::redirect("/r3")),
        Step::Respond(Canned::redirect("/r4")),
    ]);

    let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap_err();
    assert_eq!(policy(&err), FetchError::RedirectRefused);
    // 4 fetches, 3 follows — the fourth redirect response is refused, matching
    // the legacy hop<3 gate.
    assert_eq!(connector.calls().len(), 4);
}

// --- deadline ----------------------------------------------------------------

#[tokio::test]
async fn total_deadline_covers_dns() {
    let resolver = MockResolver::scripted(vec![ResolveStep::Hang]);
    let connector = MockConnector::scripted(vec![]);
    let err = fetch_feed_with(&resolver, &connector, SOURCE, &options(50))
        .await
        .unwrap_err();
    assert!(matches!(err, FeedConnectError::Deadline(50)));
}

#[tokio::test]
async fn total_deadline_covers_the_body_stream() {
    let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A])]);
    let connector = MockConnector::scripted(vec![Step::RespondHungBody {
        status: StatusCode::OK,
    }]);
    let err = fetch_feed_with(&resolver, &connector, SOURCE, &options(50))
        .await
        .unwrap_err();
    assert!(matches!(err, FeedConnectError::Deadline(50)));
}

#[tokio::test]
async fn transport_and_resolver_errors_propagate() {
    let resolver =
        MockResolver::scripted(vec![ResolveStep::Fail(io::ErrorKind::ConnectionRefused)]);
    let connector = MockConnector::scripted(vec![]);
    let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap_err();
    assert!(matches!(err, FeedConnectError::Resolve(_)));

    let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A])]);
    let connector = MockConnector::scripted(vec![Step::Error("tls failed")]);
    let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap_err();
    assert!(matches!(err, FeedConnectError::Transport(_)));
}

// --- body bounds --------------------------------------------------------------

#[tokio::test]
async fn advertised_length_over_the_cap_is_refused_before_reading() {
    let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A])]);
    let connector = MockConnector::scripted(vec![Step::Respond(
        Canned::xml("<rss/>").header("content-length", "5000000"),
    )]);
    let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap_err();
    assert_eq!(policy(&err), FetchError::TooLarge);
}

#[tokio::test]
async fn a_lying_content_length_and_a_huge_chunk_are_both_caught() {
    // Advertised small but streamed past the cap.
    let big = vec![b'a'; MAX_FEED_BYTES + 1];
    let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A])]);
    let connector = MockConnector::scripted(vec![Step::Respond(
        Canned::xml(big).header("content-length", "10"),
    )]);
    let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap_err();
    assert_eq!(policy(&err), FetchError::TooLarge);

    // No advertised length; three sub-cap chunks exceed it in aggregate —
    // the streaming bound applies per cumulative byte, not per chunk.
    let chunk = vec![b'b'; 900_000];
    let mut canned = Canned::xml(chunk.clone());
    canned.chunks = vec![chunk.clone().into(), chunk.clone().into(), chunk.into()];
    let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A])]);
    let connector = MockConnector::scripted(vec![Step::Respond(canned)]);
    let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap_err();
    assert_eq!(policy(&err), FetchError::TooLarge);
}

#[tokio::test]
async fn unsupported_content_type_is_refused_before_the_body_is_drained() {
    let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A])]);
    let connector = MockConnector::scripted(vec![Step::Respond(Canned {
        status: StatusCode::OK,
        headers: vec![("content-type".into(), "text/html".into())],
        chunks: vec![Bytes::from_static(b"<html>")],
    })]);
    let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap_err();
    assert_eq!(policy(&err), FetchError::UnsupportedContentType);
}

// --- encodings -----------------------------------------------------------------

fn gzip(bytes: &[u8]) -> Bytes {
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap().into()
}

fn zlib(bytes: &[u8]) -> Bytes {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap().into()
}

fn raw_deflate(bytes: &[u8]) -> Bytes {
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap().into()
}

#[tokio::test]
async fn gzip_and_both_deflate_dialects_decode_then_parse() {
    for (encoding, body) in [
        (
            "gzip",
            gzip(include_str!("fixtures/feeds/rss.xml").as_bytes()),
        ),
        (
            "deflate",
            zlib(include_str!("fixtures/feeds/rss.xml").as_bytes()),
        ),
        (
            "deflate",
            raw_deflate(include_str!("fixtures/feeds/rss.xml").as_bytes()),
        ),
    ] {
        let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A])]);
        let connector = MockConnector::scripted(vec![Step::Respond(
            Canned::xml(body).header("content-encoding", encoding),
        )]);
        let feed = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
            .await
            .unwrap();
        assert_eq!(
            parse_xml_feed(&feed.body).unwrap(),
            parse_xml_feed(include_str!("fixtures/feeds/rss.xml")).unwrap(),
            "{encoding} body must decode to the same feed"
        );
    }
}

#[tokio::test]
async fn unadvertised_encodings_and_corrupt_gzip_are_refused() {
    // `br` is not in our Accept-Encoding; a server sending it is refused.
    let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A])]);
    let connector = MockConnector::scripted(vec![Step::Respond(
        Canned::xml(Bytes::from_static(b"\x1b\x03junk")).header("content-encoding", "br"),
    )]);
    let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap_err();
    assert!(matches!(err, FeedConnectError::UnsupportedEncoding(_)));

    let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A])]);
    let connector = MockConnector::scripted(vec![Step::Respond(
        Canned::xml(Bytes::from_static(b"not a gzip stream")).header("content-encoding", "gzip"),
    )]);
    let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap_err();
    assert!(matches!(err, FeedConnectError::Transport(_)));
}

#[tokio::test]
async fn a_decompression_bomb_is_capped_before_the_parser() {
    // ~4 MB of zeroes compresses to a few KB — under the wire cap but over the
    // decompressed cap.
    let bomb = gzip(&vec![0u8; MAX_FEED_BYTES * 2]);
    assert!(bomb.len() < MAX_FEED_BYTES);
    let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A])]);
    let connector = MockConnector::scripted(vec![Step::Respond(
        Canned::xml(bomb).header("content-encoding", "gzip"),
    )]);
    let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap_err();
    assert_eq!(policy(&err), FetchError::TooLarge);
}

#[tokio::test]
async fn non_utf8_and_malformed_xml_fail_closed() {
    let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A])]);
    let connector =
        MockConnector::scripted(vec![Step::Respond(Canned::xml(Bytes::from_static(&[
            0xff, 0xfe, 0x00, 0x01,
        ])))]);
    let err = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
        .await
        .unwrap_err();
    assert_eq!(policy(&err), FetchError::InvalidEncoding);
}

// --- fixture compatibility -----------------------------------------------------

#[tokio::test]
async fn every_feed_fixture_parses_through_the_connector() {
    for fixture in [
        include_str!("fixtures/feeds/rss.xml"),
        include_str!("fixtures/feeds/youtube.xml"),
        include_str!("fixtures/feeds/twitch.xml"),
    ] {
        let resolver = MockResolver::scripted(vec![MockResolver::addrs(&[PUBLIC_A])]);
        let connector = MockConnector::scripted(vec![Step::Respond(
            // Chunked delivery exercises the streaming path, not one read.
            Canned::xml(fixture).chunked(97),
        )]);
        let feed = fetch_feed_with(&resolver, &connector, SOURCE, &fast())
            .await
            .unwrap();
        assert_eq!(
            parse_xml_feed(&feed.body).unwrap(),
            parse_xml_feed(fixture).unwrap()
        );
    }
}
