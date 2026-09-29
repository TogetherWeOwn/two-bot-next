//! two-bot-next container entrypoint.
//!
//! Serves liveness (`/health`) and readiness (`/readyz`) and runs the gateway
//! shard supervisor (S3). Without `DISCORD_TOKEN` the shard stays parked and
//! `/readyz` reports `gateway: down` (HTTP 503) — the Container boots healthy
//! on staging config either way.

mod backup_cli;
mod gateway;
mod server;

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::info;
use two_bot_core::{ComponentStatus, Config};

use gateway::{
    build_pipeline, build_shard, ensure_crypto_provider, intents_from_env, run_shard, GatewayState,
};
use server::serve;

#[tokio::main]
async fn main() {
    ensure_crypto_provider();
    // Docker HEALTHCHECK probe: GET /health on the configured port and exit
    // 0/1. Kept dependency-free (std + tokio only) so the check path cannot
    // rot behind an HTTP-client upgrade.
    if std::env::args().any(|arg| arg == "--healthcheck") {
        std::process::exit(healthcheck().await);
    }

    // Operator CLI (TOG-9881): backup/restore + sealed guild-config snapshot.
    // No subcommand falls through to the gateway path below. sqlx is linked
    // (core `db` feature) so these paths can open Postgres directly.
    let cli_args: Vec<String> = std::env::args().skip(1).collect();
    if !cli_args.is_empty() && cli_args[0] != "--help" && cli_args[0] != "-h" {
        let code = backup_cli::dispatch(&cli_args).await;
        // 100 = not a backup subcommand: fall through to serve.
        if code != 100 {
            std::process::exit(code);
        }
    } else if !cli_args.is_empty() {
        print_backup_help_and_exit().await;
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

    if let Some(token) = config.discord_token.clone().filter(|t| !t.is_empty()) {
        // S5 offers the persisted session here for RESUME; fresh IDENTIFY
        // until then.
        let shard = build_shard(token, intents_from_env(), None);
        let pipeline = Arc::new(build_pipeline());
        info!("discord token present; gateway shard connecting");
        tokio::spawn(run_shard(shard, pipeline, Arc::clone(&state)));
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

/// `--help` covers both the gateway server and the backup CLI.
async fn print_backup_help_and_exit() -> ! {
    let code = backup_cli::dispatch(&["--help".to_owned()]).await;
    std::process::exit(code);
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
