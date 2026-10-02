//! Real preflight constructor path against an IPv6 loopback-only mock.
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn preflight_accepts_ipv6_loopback_without_an_admission_authority() {
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let mock = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0; 1024];
        loop {
            let count = socket.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            assert!(bytes.len() < 8192);
            if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                break;
            }
        }
        assert!(bytes.starts_with(b"GET /api/v10/users/@me "));
        socket
            .write_all(b"HTTP/1.1 403 Fixture\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .await
            .unwrap();
    });
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_two-bot"))
            .env_clear()
            .arg("preflight")
            .arg("--json")
            .env("DISCORD_TOKEN", "fixture-ipv6-token")
            .env("GUILD_ID", "2222")
            .env("TWO_ONBOARDING_MODE", "session")
            .env("DISCORD_PREFLIGHT_API_BASE", origin)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "mock REST refusal, not constructor rejection"
    );
    tokio::time::timeout(Duration::from_secs(1), mock)
        .await
        .unwrap()
        .unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-ipv6-token"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-ipv6-token"));
}
