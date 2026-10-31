//! The containment request journal must retain forbidden calls even when
//! the scripted responder panics instead of returning a response.

#[path = "support/containment_rest.rs"]
mod common;

use std::time::Duration;

use common::{MockRest, ScriptedResponse};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

async fn assert_forbidden_request_recorded(method: &str, path: &str, body: &[u8]) {
    let mock = MockRest::with_responder(|_| panic!("forbidden containment request")).await;
    let origin = mock.origin();
    let mut stream = TcpStream::connect(origin.strip_prefix("http://").expect("mock origin"))
        .await
        .expect("connect to mock");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: mock\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    timeout(Duration::from_secs(5), async {
        stream.write_all(request.as_bytes()).await.expect("headers");
        stream.write_all(body).await.expect("body");
        let mut response = Vec::new();
        // A panicking handler closes or resets the connection without a response.
        let _ = stream.read_to_end(&mut response).await;
        assert!(response.is_empty(), "a forbidden request never responds");
    })
    .await
    .expect("panic guard closes the connection promptly");

    // Reading after the panic also proves the journal mutex was not poisoned
    // by holding it across the responder callback.
    let recorded = mock.requests();
    mock.shutdown().await;
    assert_eq!(recorded.len(), 1, "panic-guarded request remains visible");
    assert_eq!(recorded[0].method, method);
    assert_eq!(recorded[0].path, path);
    assert_eq!(recorded[0].body, body);
}

#[tokio::test]
async fn forbidden_third_delete_is_recorded_despite_responder_panic() {
    assert_forbidden_request_recorded("DELETE", "/api/v10/guilds/22/members/5001/roles/80", b"")
        .await;
}

#[tokio::test]
async fn forbidden_role_restore_put_is_recorded_despite_responder_panic() {
    assert_forbidden_request_recorded("PUT", "/api/v10/guilds/22/members/5001/roles/77", b"").await;
}

#[tokio::test]
async fn forbidden_member_restore_patch_is_recorded_despite_responder_panic() {
    assert_forbidden_request_recorded(
        "PATCH",
        "/api/v10/guilds/22/members/5001",
        br#"{"roles":["77","79","80"]}"#,
    )
    .await;
}

#[tokio::test]
async fn successful_responses_still_record_requests() {
    let mock = MockRest::with_responder(|request| match request.path.as_str() {
        "/status" => ScriptedResponse::status(204),
        "/json" => ScriptedResponse::json(200, serde_json::json!({"ok": true})),
        _ => panic!("unexpected recording control route"),
    })
    .await;
    for (path, expected_status) in [("/status", "204"), ("/json", "200")] {
        let origin = mock.origin();
        let mut stream = TcpStream::connect(origin.strip_prefix("http://").expect("mock origin"))
            .await
            .expect("connect to mock");
        let response = timeout(Duration::from_secs(5), async {
            stream
                .write_all(
                    format!("GET {path} HTTP/1.1\r\nHost: mock\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .expect("request");
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .await
                .expect("response");
            response
        })
        .await
        .expect("control response promptly");
        assert!(response.starts_with(&format!("HTTP/1.1 {expected_status}")));
    }
    let recorded = mock.requests();
    mock.shutdown().await;
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].path, "/status");
    assert_eq!(recorded[1].path, "/json");
}
