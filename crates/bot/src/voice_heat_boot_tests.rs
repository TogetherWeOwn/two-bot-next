//! Child-process boot probes isolate configuration from other runtime tests.
use super::*;
use crate::{gateway::GatewayState, voice_heat_runtime::NAME};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::RwLock,
};
use two_bot_core::Secret;

const PROBE: &str = "VOICE_HEAT_BOOT_PROBE";
const PROBE_TEST: &str = "website_jobs::voice_heat_boot_tests::isolated_boot_probe";

#[test]
fn boot_admits_heat_only_with_execute_and_gateway_prerequisites_even_if_rest_refuses() {
    for (mode, retention) in [
        ("configured", Some("execute")),
        ("missing-token", Some("execute")),
        ("missing-db", Some("execute")),
        ("invalid-db", Some("execute")),
        ("missing-guild", Some("execute")),
        ("default", None),
        ("dry-run", Some("dry-run")),
        ("off", Some("off")),
        ("invalid-mode", Some("EXECUTE")),
    ] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .args([PROBE_TEST, "--exact", "--nocapture"])
            .env(PROBE, mode)
            .env("DISCORD_API_BASE", "not a REST origin")
            .env("TWO_DATABASE_TLS", "local-only");
        if let Some(retention) = retention {
            command.env(crate::retention_jobs::MODE_SETTING, retention);
        }
        let output = command
            .output()
            .expect("run isolated voice heat boot probe");
        assert!(
            output.status.success(),
            "boot mode {mode} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("running 1 test"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("voice heat boot probe completed"));
    }
}

#[tokio::test]
async fn isolated_boot_probe() {
    let Ok(mode) = std::env::var(PROBE) else {
        return;
    };
    crate::gateway::ensure_crypto_provider();
    // Lazy and deliberately unreachable; registration requires no DB I/O.
    let url = if mode == "invalid-db" {
        "not a database URL"
    } else {
        "postgres://fixture@127.0.0.1:1/fixture"
    };
    let config = Config {
        discord_token: (mode != "missing-token")
            .then(|| Secret::new("synthetic-voice-heat-token".to_owned())),
        database_url: (mode != "missing-db").then(|| Secret::new(url.to_owned())),
        guild_id: (mode != "missing-guild").then_some(2222),
        listen_addr: "127.0.0.1:0".into(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, _) = watch::channel(false);
    let shutdown = stop.clone();
    let gateway = server::SharedState::new(Arc::new(RwLock::new(GatewayState::Connected)), None);
    let owner = tokio::spawn(async move {
        serve(
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
    let response: Value = tokio::time::timeout(Duration::from_secs(5), async {
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket
            .write_all(b"GET /readyz HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        let (_, body) = response.split_once("\r\n\r\n").expect("HTTP readiness");
        serde_json::from_str(body).expect("readiness JSON")
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
    assert_eq!(
        jobs["retention"]["parked"],
        matches!(
            mode.as_str(),
            "missing-db" | "invalid-db" | "off" | "invalid-mode"
        ),
        "retention admission remains independent of the gateway"
    );
    if mode != "configured" {
        assert!(jobs[NAME]["last_start"].is_null());
    }
    assert_eq!(jobs["counter"]["parked"], true, "REST jobs stay parked");
    assert_eq!(jobs["rank"]["parked"], true);
    assert_eq!(jobs["scheduled_events"]["parked"], true);
    eprintln!("voice heat boot probe completed");
}
