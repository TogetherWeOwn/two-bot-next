//! Child-process boot probes keep REST configuration out of sibling tests.
use super::*;
use crate::{gateway::GatewayState, voice_heat::NAME};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::RwLock,
};
use two_bot_core::{Config, Secret};

const PROBE: &str = "VOICE_HEAT_BOOT_PROBE";
const PROBE_TEST: &str = "website_jobs::voice_heat_boot_tests::isolated_boot_probe";

#[test]
fn rest_failure_does_not_park_heat_but_missing_gateway_prerequisites_do() {
    for mode in ["configured", "missing-token", "missing-db", "missing-guild"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .args([PROBE_TEST, "--exact", "--nocapture"])
            .env(PROBE, mode)
            .env("DISCORD_API_BASE", "not a REST origin")
            .env("TWO_DATABASE_TLS", "local-only")
            .env(crate::retention_jobs::MODE_SETTING, "off")
            .output()
            .expect("run isolated voice-heat boot probe");
        assert!(
            output.status.success(),
            "boot mode {mode} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("running 1 test"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("voice-heat boot probe completed"));
    }
}

#[tokio::test]
async fn isolated_boot_probe() {
    let Ok(mode) = std::env::var(PROBE) else {
        return;
    };
    crate::gateway::ensure_crypto_provider();
    // Lazy, deliberately unreachable fixture pool; no database is needed by
    // registration or a sampler with an incomplete gateway cache.
    let url = "postgres://fixture:fixture-password@127.0.0.1:1/fixture";
    let pool = crate::website_jobs::admission_pool_with_tls(
        url,
        two_bot_core::database_tls::TlsPolicy::LocalOnly,
    )
    .unwrap();
    assert_eq!(pool.size(), 0);
    assert!(crate::website_jobs::governed_executor(
        "synthetic-voice-heat-token",
        std::env::var("DISCORD_API_BASE").ok(),
        pool.clone(),
    )
    .is_err());
    pool.close().await;
    let config = Config {
        discord_token: (mode != "missing-token")
            .then(|| Secret::new("synthetic-voice-heat-token".to_owned())),
        database_url: (mode != "missing-db").then(|| Secret::new(url.to_owned())),
        guild_id: (mode != "missing-guild").then_some(2222),
        listen_addr: "127.0.0.1:0".into(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, _) = tokio::sync::watch::channel(false);
    let shutdown = stop.clone();
    let gateway =
        crate::server::SharedState::new(Arc::new(RwLock::new(GatewayState::Connected)), None);
    let owner = tokio::spawn(async move {
        crate::website_jobs::serve(
            &config,
            listener,
            gateway,
            shutdown,
            None,
            Arc::new(std::sync::OnceLock::new()),
            None,
            None,
            None,
            crate::voice_snapshot::new_shared_cache(),
        )
        .await
    });
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket
            .write_all(b"GET /readyz HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        let (_, body) = response.split_once("\r\n\r\n").expect("HTTP readiness");
        serde_json::from_str::<serde_json::Value>(body).expect("readiness JSON")
    })
    .await
    .expect("boot readiness must answer promptly");
    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(5), owner)
        .await
        .expect("boot owner joins shutdown")
        .unwrap()
        .unwrap();
    let jobs = &response["jobs"];
    assert!(jobs[NAME].is_object(), "heat readiness entry: {response}");
    assert_eq!(jobs[NAME]["parked"], mode != "configured");
    assert_eq!(jobs["counter"]["parked"], true, "REST jobs stay parked");
    assert_eq!(jobs["rank"]["parked"], true);
    assert_eq!(jobs["scheduled_events"]["parked"], true);
    eprintln!("voice-heat boot probe completed");
}
