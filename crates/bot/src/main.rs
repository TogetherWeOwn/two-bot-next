//! two-bot-next container entrypoint.
//!
//! S1 skeleton: serves liveness (`/health`) and readiness (`/readyz`) and
//! owns the gateway shard supervisor seam. Without `DISCORD_TOKEN` the shard
//! stays parked and `/readyz` reports `gateway: down` (HTTP 503) — the
//! Container boots healthy on staging config either way.

mod gateway;
mod server;

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::info;
use two_bot_core::{ComponentStatus, Config};

use gateway::GatewayState;
use server::serve;

#[tokio::main]
async fn main() {
    // Docker HEALTHCHECK probe: GET /health on the configured port and exit
    // 0/1. Kept dependency-free (std + tokio only) so the check path cannot
    // rot behind an HTTP-client upgrade.
    if std::env::args().any(|arg| arg == "--healthcheck") {
        std::process::exit(healthcheck().await);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "two_bot=info".into()),
        )
        .init();

    let config = Config::from_env().unwrap_or_else(|err| {
        tracing::warn!(error = %err, "config invalid; continuing with safe defaults");
        Config {
            discord_token: None,
            database_url: None,
            listen_addr: "0.0.0.0:8080".to_owned(),
            guild_id: None,
        }
    });

    let state = Arc::new(RwLock::new(GatewayState::new(&config)));

    if config.gateway_configured() {
        info!("discord token present; gateway supervisor armed (S3 connects)");
    } else {
        info!(
            status = ?ComponentStatus::Down,
            "no discord token; gateway parked, /readyz reports down"
        );
    }

    if let Err(err) = serve(&config.listen_addr, state).await {
        tracing::error!(error = %err, "http server failed");
        std::process::exit(1);
    }
}

/// Probe /health over plain HTTP using only tokio (no client dependency).
/// Returns the process exit code: 0 when the local server answers 200.
async fn healthcheck() -> i32 {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_owned());
    // Inside the container 0.0.0.0 is not diallable; loopback is equivalent.
    let addr = addr.replacen("0.0.0.0", "127.0.0.1", 1);
    let mut stream = match tokio::net::TcpStream::connect(&addr).await {
        Ok(stream) => stream,
        Err(_) => return 1,
    };
    let request = b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n";
    if stream.write_all(request).await.is_err() {
        return 1;
    }
    let mut response = Vec::with_capacity(128);
    if stream.read_to_end(&mut response).await.is_err() {
        return 1;
    }
    if response.starts_with(b"HTTP/1.0 200") || response.starts_with(b"HTTP/1.1 200") {
        0
    } else {
        1
    }
}
