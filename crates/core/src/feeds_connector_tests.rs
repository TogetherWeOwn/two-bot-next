//! In-module unit tests for the pinned dial path. `PinnedResolver`'s fields
//! are private and its only production constructor takes a validated
//! `PublicRequest`; `for_test` is `#[cfg(test)]`-only so loopback fixture
//! addresses can never appear in a production build.

use super::*;

use std::str::FromStr;

use http::Uri;
use hyper_rustls::{DefaultServerNameResolver, ResolveServerName};

fn name(host: &str) -> Name {
    Name::from_str(host).expect("valid dns name")
}

#[tokio::test]
async fn pinned_resolver_serves_only_the_pinned_addresses() {
    let addrs: Vec<SocketAddr> = vec![
        "93.184.216.34:443".parse().unwrap(),
        "[2606:2800:220:1:248:1893:25c8:1946]:443".parse().unwrap(),
    ];
    // Case and trailing-dot differences must not break the match — the DNS
    // layer is case-insensitive and tolerates root dots.
    let mut resolver = PinnedResolver::for_test("Example.org", &addrs);
    let dial = <PinnedResolver as Service<Name>>::call(&mut resolver, name("EXAMPLE.ORG."))
        .await
        .expect("pinned name resolves");
    assert_eq!(dial.collect::<Vec<_>>(), addrs);
}

#[tokio::test]
async fn pinned_resolver_refuses_any_other_name() {
    let mut resolver =
        PinnedResolver::for_test("example.org", &["93.184.216.34:443".parse().unwrap()]);
    for other in ["other.example.org", "example.org.evil.test", "localhost"] {
        assert!(
            <PinnedResolver as Service<Name>>::call(&mut resolver, name(other))
                .await
                .is_err(),
            "{other} must never resolve"
        );
    }
}

/// The real hyper dial path, end to end but without TLS: `feed.invalid` does
/// not resolve publicly, yet the connection must land on the pinned loopback
/// listener — and nowhere else. (`HttpConnector` rewrites a resolver port to
/// the Uri's explicit port, so the Uri carries the pinned port here exactly
/// as `PublicRequest` pre-computes it in production.)
#[tokio::test]
async fn http_connector_dials_the_pinned_socket_not_dns() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pinned = listener.local_addr().unwrap();
    let mut connector =
        HttpConnector::new_with_resolver(PinnedResolver::for_test("feed.invalid", &[pinned]));
    connector.enforce_http(false);

    let accept = tokio::spawn(async move { listener.accept().await.unwrap() });
    let uri: Uri = format!("http://feed.invalid:{}/feed.xml", pinned.port())
        .parse()
        .unwrap();
    let stream = connector
        .call(uri)
        .await
        .expect("pinned dial must reach the listener");
    let (accepted, _peer) = accept.await.unwrap();
    assert_eq!(
        stream.into_inner().peer_addr().unwrap(),
        accepted.local_addr().unwrap(),
        "the dialled peer is exactly the pinned socket"
    );
}

#[tokio::test]
async fn http_connector_refuses_a_uri_host_it_did_not_pin() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pinned = listener.local_addr().unwrap();
    let mut connector =
        HttpConnector::new_with_resolver(PinnedResolver::for_test("feed.invalid", &[pinned]));
    connector.enforce_http(false);
    let err = connector
        .call(Uri::from_static("http://sibling.invalid/"))
        .await
        .expect_err("a non-pinned host must not dial");
    let cause = std::error::Error::source(&err)
        .map(ToString::to_string)
        .unwrap_or_default();
    assert!(
        cause.contains("refused"),
        "expected the pinned resolver's refusal, got {err:?} (cause: {cause})"
    );
}

/// TLS identity comes from the URL host, not from any pinned address —
/// `DefaultServerNameResolver` derives the SNI `ServerName` from `uri.host()`.
#[test]
fn server_name_resolution_uses_url_host() {
    let resolver = DefaultServerNameResolver::default();
    let uri = Uri::from_static("https://example.org/feed.xml");
    let resolved = resolver.resolve(&uri).expect("host parses as DNS name");
    let debug = format!("{resolved:?}");
    assert!(
        debug.contains("example.org"),
        "SNI server name must be the URL host, got {debug}"
    );
}

/// `https_only` refuses a cleartext Uri before any resolver/dial activity —
/// scheme downgrade is impossible in the transport itself.
#[tokio::test]
async fn https_only_transport_refuses_cleartext() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pinned = listener.local_addr().unwrap();
    let mut http =
        HttpConnector::new_with_resolver(PinnedResolver::for_test("feed.invalid", &[pinned]));
    http.enforce_http(false);
    let mut https = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_only()
        .enable_http1()
        .wrap_connector(http);
    let err = Service::call(&mut https, Uri::from_static("http://feed.invalid/"))
        .await
        .expect_err("cleartext must be refused");
    let _ = err;
    // Prove nothing was dialled: the listener must sit idle.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err(),
        "refused request must not open a socket"
    );
}

/// The wire request carries the legacy identity headers and keeps the URL
/// authority intact (Host header derives from the Uri on send).
#[test]
fn request_carries_legacy_headers_and_url_authority() {
    let url = Url::parse("https://example.org/feed?x=1").unwrap();
    let req = PinnedHttpsConnector::build_request(&url).unwrap();
    assert_eq!(req.uri().host(), Some("example.org"));
    assert_eq!(req.uri().scheme_str(), Some("https"));
    assert_eq!(req.headers()[USER_AGENT], FEED_USER_AGENT);
    assert_eq!(req.headers()[ACCEPT_ENCODING], ACCEPT_ENCODING_VALUE);
}

#[test]
fn normalized_host_strips_case_dot_and_brackets() {
    assert_eq!(normalized_host("FEEDS.Example.ORG."), "feeds.example.org");
    assert_eq!(normalized_host("[2606:2800::1]"), "2606:2800::1");
}
