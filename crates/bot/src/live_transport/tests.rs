use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{http::HeaderMap, Router};
use http_body_util::BodyExt as _;
use hyper::Method;
use time::format_description::well_known::Rfc3339;
use tokio::net::TcpListener;
use two_bot_core::realtime_wire::{
    self as wire, Change, Channel, ChannelKind, Id, Operation, WireTime,
};

use super::*;
use crate::live_delivery;

const ORIGIN: &str = "https://site.example";
const KEY_ID: &str = "ingress-fixture";
const KEY: &str = "offline transport fixture";
const WEB_DELTA: &str = include_str!("fixtures/web_delta.json");

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
        assert!(LiveTransport::new(origin, KEY_ID, Secret::new(KEY.to_owned())).is_none());
    }
    for key_id in [
        "",
        "not ascii é",
        "has space",
        "bad\nheader",
        &"a".repeat(129),
    ] {
        assert!(LiveTransport::new(ORIGIN, key_id, Secret::new(KEY.to_owned())).is_none());
    }
    assert!(LiveTransport::new(ORIGIN, KEY_ID, Secret::new(String::new())).is_none());

    let transport = LiveTransport::new(
        "https://site.example:8443/",
        "A_b.1:-",
        Secret::new(KEY.to_owned()),
    )
    .unwrap();
    assert_eq!(
        transport.endpoint.expose().authority().unwrap().as_str(),
        "site.example:8443"
    );
    assert_eq!(transport.endpoint.expose().path(), INGEST_PATH);
    for debug in [format!("{transport:?}"), format!("{transport:#?}")] {
        assert!(!debug.contains(KEY));
        assert!(!debug.contains("A_b.1:-"));
    }
}

#[tokio::test]
async fn request_matches_web_verified_wire_fixture() {
    let transport = LiveTransport::new(ORIGIN, KEY_ID, Secret::new(KEY.to_owned())).unwrap();
    let bytes = delta(41);
    let request = transport
        .request(&bytes, 1_800_000_000, [0xab; 16])
        .unwrap();
    assert_eq!(request.method(), Method::POST);
    assert_eq!(request.uri().to_string(), format!("{ORIGIN}{INGEST_PATH}"));
    assert_eq!(request.headers()["content-type"], "application/json");
    assert_eq!(request.headers()["x-two-key-id"], KEY_ID);
    assert_eq!(request.headers()["x-two-timestamp"], "1800000000");
    assert_eq!(request.headers()["x-two-nonce"], "ab".repeat(16).as_str());
    assert_eq!(
        request.headers()["x-two-signature"],
        "sha256=1a1c4ced824361c8e7a08b56ea1da47c9d0a112fe7a8ad6488ed6690ecd3500e"
    );
    assert!(request.headers()["x-two-signature"].is_sensitive());
    assert_eq!(
        request.into_body().collect().await.unwrap().to_bytes(),
        WEB_DELTA.trim_end().as_bytes()
    );
}

fn loopback_transport(listener: &TcpListener) -> LiveTransport {
    let mut transport = LiveTransport::new(ORIGIN, KEY_ID, Secret::new(KEY.to_owned())).unwrap();
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
    let transport = loopback_transport(&listener);
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
        assert_eq!(uri.path_and_query().unwrap().as_str(), INGEST_PATH);
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["x-two-key-id"], KEY_ID);
        assert_eq!(bytes.as_ref(), delta(seq).as_slice());
        let mut nonce = [0; 16];
        let nonce_hex = headers["x-two-nonce"].to_str().unwrap();
        hex::decode_to_slice(nonce_hex, &mut nonce).unwrap();
        assert!(nonces.insert(nonce));
        let timestamp = headers["x-two-timestamp"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let expected = sign_ingest(&Secret::new(KEY.to_owned()), timestamp, nonce, bytes).unwrap();
        assert_eq!(nonce_hex, expected.nonce);
        assert_eq!(
            headers["x-two-signature"],
            expected.signature.expose().as_str()
        );
    }
}

#[tokio::test]
async fn connection_closed_without_headers_is_retryable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let transport = loopback_transport(&listener);
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
