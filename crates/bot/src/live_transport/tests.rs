use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{http::HeaderMap, Router};
use hmac::{Hmac, KeyInit, Mac as _};
use http_body_util::BodyExt as _;
use hyper::Method;
use sha2::{Digest as _, Sha256};
use time::format_description::well_known::Rfc3339;
use tokio::net::TcpListener;
use two_bot_core::realtime_wire::{
    self as wire, Change, Channel, ChannelKind, Id, Operation, WireTime,
};

use super::*;
use crate::live_delivery;

const ORIGIN: &str = "https://site.example";
pub(crate) const KEY_ID: &str = "ingress-fixture";
const WEB_PATH: &str = "/internal/discord-live/v1/ingest";
const WEB_DELTA: &str = include_str!("fixtures/web_delta.json");

fn fixture_key() -> Secret<String> {
    Secret::new(hex::encode(rand::random::<[u8; 32]>()))
}

pub(crate) fn web_signature(
    key: &Secret<String>,
    timestamp: &str,
    nonce: &str,
    body: &[u8],
) -> String {
    // Match the Web verifier's canonical string independently of sign_ingest.
    let digest = hex::encode(Sha256::digest(body));
    let mut mac = Hmac::<Sha256>::new_from_slice(key.expose().as_bytes()).unwrap();
    mac.update(format!("POST\n{WEB_PATH}\n{timestamp}\n{nonce}\n{digest}").as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn delta(seq: u64) -> Vec<u8> {
    let guild_id = Id(100_u64.try_into().unwrap());
    let room_id = Id(200_u64.try_into().unwrap());
    let occurred_at = OffsetDateTime::parse("2026-11-20T06:00:00Z", &Rfc3339).unwrap();
    let operation = Operation::Channels(Change::Upsert {
        id: room_id,
        value: Channel {
            id: room_id,
            name: "Café".to_owned(),
            kind: ChannelKind::Voice,
            parent_id: None,
            position: None,
            overwrites: vec![],
        },
    });
    wire::encode_delta(guild_id, seq, WireTime(occurred_at), &[operation]).unwrap()
}

#[test]
fn accepts_only_canonical_https_origins_and_valid_ingress_keys() {
    let key = fixture_key();
    for origin in [
        "http://site.example",
        "https://site.example/a",
        "https://site.example/a/..",
        "https://site.example/?",
        "https://site.example/#",
        "https://user:pass@site.example",
        "https://@site.example",
        "https://site.example//",
        "site.example",
        "https://site.example:bad",
    ] {
        assert!(LiveTransport::new(origin, KEY_ID, key.clone()).is_none());
    }
    for key_id in [
        "",
        "not ascii é",
        "has space",
        "bad\nheader",
        &"a".repeat(129),
    ] {
        assert!(LiveTransport::new(ORIGIN, key_id, key.clone()).is_none());
    }
    assert!(LiveTransport::new(ORIGIN, KEY_ID, Secret::new(String::new())).is_none());

    let transport =
        LiveTransport::new("https://site.example:8443/", "A_b.1:-", key.clone()).unwrap();
    assert_eq!(
        transport.endpoint.expose().authority().unwrap().as_str(),
        "site.example:8443"
    );
    assert_eq!(transport.endpoint.expose().path(), WEB_PATH);
    for debug in [format!("{transport:?}"), format!("{transport:#?}")] {
        assert!(!debug.contains(key.expose().as_str()));
        assert!(!debug.contains("A_b.1:-"));
    }
}

#[tokio::test]
async fn request_matches_web_verified_wire_fixture() {
    let key = fixture_key();
    let transport = LiveTransport::new(ORIGIN, KEY_ID, key.clone()).unwrap();
    let bytes = delta(41);
    let nonce = rand::random::<[u8; 16]>();
    let request = transport.request(&bytes, 1_800_000_000, nonce).unwrap();
    let nonce_hex = hex::encode(nonce);
    let web_body = WEB_DELTA.trim_end().as_bytes();
    let expected = web_signature(&key, "1800000000", &nonce_hex, web_body);
    assert_eq!(request.method(), Method::POST);
    assert_eq!(request.uri().to_string(), format!("{ORIGIN}{WEB_PATH}"));
    assert_eq!(request.headers()["content-type"], "application/json");
    assert_eq!(request.headers()["x-two-key-id"], KEY_ID);
    assert_eq!(request.headers()["x-two-timestamp"], "1800000000");
    assert_eq!(request.headers()["x-two-nonce"], nonce_hex.as_str());
    assert_eq!(request.headers()["x-two-signature"], expected.as_str());
    assert!(request.headers()["x-two-signature"].is_sensitive());
    assert_eq!(
        request.into_body().collect().await.unwrap().to_bytes(),
        web_body
    );
}

pub(crate) fn loopback_transport(listener: &TcpListener, key: Secret<String>) -> LiveTransport {
    let mut transport = LiveTransport::new(ORIGIN, KEY_ID, key).unwrap();
    transport.endpoint = Secret::new(
        format!("http://{}{INGEST_PATH}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    );
    transport.client = build_client(
        HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build(),
    );
    transport
}

#[tokio::test]
async fn worker_retries_the_same_numbered_bytes_and_stops_on_refusal() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let key = fixture_key();
    let transport = loopback_transport(&listener, key.clone());
    let observed = Arc::new(Mutex::new(Vec::new()));
    let fixture = Arc::clone(&observed);
    let router = Router::new().fallback(
        move |method: Method, uri: Uri, headers: HeaderMap, bytes: Bytes| {
            let fixture = Arc::clone(&fixture);
            async move {
                let mut requests = fixture.lock().unwrap();
                let status = [
                    StatusCode::SERVICE_UNAVAILABLE,
                    StatusCode::TOO_MANY_REQUESTS,
                    StatusCode::NO_CONTENT,
                    StatusCode::CONFLICT,
                    StatusCode::TEMPORARY_REDIRECT,
                    StatusCode::UNAUTHORIZED,
                    StatusCode::OK,
                ]
                .get(requests.len())
                .copied()
                .unwrap_or(StatusCode::BAD_REQUEST);
                requests.push((method, uri, headers, bytes));
                (status, [("location", "/unexpected-redirect")])
            }
        },
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await });

    let (sender, receiver) = live_delivery::channel::<Vec<u8>>();
    for seq in 41..=45 {
        assert!(sender.offer(delta(seq)));
    }
    drop(sender);
    let mut verdicts = Vec::new();
    receiver
        .run(async |body| {
            let result = transport.attempt(body).await;
            verdicts.push(result);
            result
        })
        .await;
    assert!(
        verdicts
            == [
                Delivery::Retry,
                Delivery::Retry,
                Delivery::Accepted,
                Delivery::Rejected,
                Delivery::Rejected,
                Delivery::Rejected,
                Delivery::Rejected,
            ]
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());

    let observed = observed.lock().unwrap();
    assert_eq!(observed.len(), 7);
    let mut nonces = HashSet::new();
    for ((method, uri, headers, bytes), seq) in observed.iter().zip([41, 41, 41, 42, 43, 44, 45]) {
        assert_eq!(*method, Method::POST);
        assert_eq!(uri.path_and_query().unwrap().as_str(), WEB_PATH);
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["x-two-key-id"], KEY_ID);
        assert_eq!(bytes.as_ref(), delta(seq).as_slice());
        let nonce_hex = headers["x-two-nonce"].to_str().unwrap();
        let nonce = hex::decode(nonce_hex).unwrap();
        assert_eq!(nonce.len(), 16);
        assert_eq!(hex::encode(&nonce), nonce_hex);
        assert!(nonces.insert(nonce));
        let timestamp = headers["x-two-timestamp"].to_str().unwrap();
        let seconds: i64 = timestamp.parse().unwrap();
        assert!((1..=999_999_999_999_999).contains(&seconds));
        assert_eq!(timestamp, seconds.to_string());
        let expected = web_signature(&key, timestamp, nonce_hex, bytes);
        assert_eq!(headers["x-two-signature"], expected.as_str());
    }
}

#[tokio::test]
async fn connection_closed_without_headers_is_retryable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let transport = loopback_transport(&listener, fixture_key());
    let body = delta(41);
    let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(transport.attempt(&body), async {
            drop(listener.accept().await.unwrap());
        })
    })
    .await
    .unwrap();
    assert!(result == Delivery::Retry);
}
