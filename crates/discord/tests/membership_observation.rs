//! REST boundary contracts from two-bot@bffccf3; all HTTP is loopback fixtures.
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use two_bot_discord::ActionExecutor;

fn ms(value: &str) -> i64 {
    two_bot_core::parse_iso_millis(value).expect("timestamp")
}

// Source: two-bot@bffccf3 test/unit.membership-rest-observation.test.ts:9.
#[tokio::test]
async fn roster_evidence_precedes_delayed_headers_and_body() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0; 4096];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&buf[..n]);
        }
        assert!(String::from_utf8_lossy(&request)
            .starts_with("GET /api/v10/guilds/2222/members?limit=1000 "));
        tokio::time::sleep(Duration::from_millis(30)).await;
        let headers_at = two_bot_core::now_iso();
        let body = r#"[{"user":{"id":"3333"},"joined_at":"2026-09-29T00:00:00.000Z"}]"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let body_at = two_bot_core::now_iso();
        stream.write_all(body.as_bytes()).await.unwrap();
        (headers_at, body_at)
    });
    let exec = ActionExecutor::with_proxy("fixture-token".into(), Some(origin)).unwrap();
    let (data, observed_at) = exec
        .get_json_observed("/guilds/2222/members?limit=1000")
        .await
        .unwrap()
        .unwrap();
    let (headers_at, body_at) = server.await.unwrap();
    assert_eq!(data[0]["user"]["id"], "3333");
    assert!(ms(&observed_at) < ms(&headers_at));
    assert!(ms(&headers_at) < ms(&body_at));
    assert_eq!(exec.requests(), 1);
}

// Source: two-bot@bffccf3 test/unit.membership-rest-observation.test.ts:31.
#[tokio::test]
async fn retried_roster_evidence_uses_successful_attempt_start() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::status(503),
            ScriptedResponse::json(200, serde_json::json!([{"user":{"id":"3333"}}])),
        ],
        ScriptedResponse::status(403),
    )
    .await;
    let exec = ActionExecutor::with_proxy("fixture-token".into(), Some(mock.origin())).unwrap();
    let began = two_bot_core::now_iso();
    let (data, observed_at) = exec
        .get_json_observed("/guilds/2222/members?limit=1000")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(data[0]["user"]["id"], "3333");
    assert!(
        ms(&observed_at) - ms(&began) >= 500,
        "failed attempt's timestamp must not survive retry"
    );
    assert_eq!(exec.requests(), 2);
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

// Source: two-bot@bffccf3 test/unit.membership-rest-observation.test.ts:55.
#[tokio::test]
async fn failed_roster_page_has_no_evidence_and_ordinary_get_is_data_only() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(403)],
        ScriptedResponse::json(200, serde_json::json!({"value":1})),
    )
    .await;
    let exec = ActionExecutor::with_proxy("fixture-token".into(), Some(mock.origin())).unwrap();
    assert!(exec
        .get_json_observed("/guilds/2222/members?limit=1000")
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        exec.get_json("/channels/4444").await.unwrap(),
        Some(serde_json::json!({"value":1}))
    );
    mock.shutdown().await;
}
