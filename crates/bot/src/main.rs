//! two-bot-next container entrypoint.
//!
//! Serves liveness (`/health`) and readiness (`/readyz`) and runs the gateway
//! shard supervisor (S3). Without `DISCORD_TOKEN`, `DATABASE_URL`, or a nonzero
//! `GUILD_ID` the shard stays parked and `/readyz` reports `gateway: down`
//! (HTTP 503) — the Container boots healthy on incomplete staging config.

mod backup_cli;
mod dispatch;
mod gateway;
#[cfg(test)]
mod gateway_tests;
#[cfg(test)]
mod lifecycle_tests;
mod server;

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::info;
use two_bot_core::{ComponentStatus, Config};

use gateway::{
    build_persistent_pipeline, build_shard, ensure_crypto_provider, intents_from_env, run_shard,
    GatewayState,
};
use server::{serve, SharedState};

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
            // Gateway config errors must not move the listener away from
            // the Worker probes and Docker healthcheck's configured address.
            listen_addr: std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_owned()),
            guild_id: None,
        }
    });

    let gateway = Arc::new(RwLock::new(GatewayState::new(&config)));
    let store = match gateway_prerequisites(&config).ok().map(|(_, url, _)| url) {
        Some(url) => match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            two_bot_store::Store::connect(url, false),
        )
        .await
        {
            Ok(Ok(store)) => Some(store),
            _ => {
                tracing::error!("database initialization failed; exiting for supervisor restart");
                std::process::exit(1);
            }
        },
        None => None,
    };
    let state = SharedState {
        gateway: Arc::clone(&gateway),
        database: store.as_ref().map(|s| s.pool().clone()),
    };

    let (shutdown, mut stopping) = tokio::sync::watch::channel(false);
    let gateway_task = if let Ok((token, _, guild_id)) = gateway_prerequisites(&config) {
        let token = token.to_owned();
        let state = Arc::clone(&gateway);
        Some(tokio::spawn(async move {
            let result: Result<(), sqlx::Error> = async {
                let db = store.ok_or_else(|| {
                    sqlx::Error::InvalidArgument(
                        "DATABASE_URL required for gateway checkpoint".into(),
                    )
                })?;
                let pool = db.pool().clone();
                tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    sqlx::migrate!("../cutover/migrations").run(&pool),
                )
                .await
                .map_err(|_| {
                    sqlx::Error::InvalidArgument("gateway migration deadline exceeded".into())
                })?
                .map_err(|_| sqlx::Error::InvalidArgument("gateway migration failed".into()))?;
                let store = two_bot_cutover::gateway_session::GatewaySessionStore::new(
                    pool,
                    guild_id.to_string(),
                    0,
                );
                let saved = gateway::load_boot_session(&store).await?;
                let pipeline =
                    Arc::new(build_persistent_pipeline(&store, guild_id, token.clone()).await?);
                let shard = build_shard(token, intents_from_env(), saved.as_ref());
                info!(
                    resume = saved.is_some(),
                    "durable gateway initialized; shard connecting"
                );
                run_shard(shard, pipeline, Arc::clone(&state), store, async move {
                    let _ = stopping.wait_for(|stopping| *stopping).await;
                })
                .await
            }
            .await;
            if result.is_err() {
                // Do not print sqlx errors: configuration errors may contain a URL.
                tracing::error!(
                    "durable gateway failed; checkpoint unchanged, readiness unavailable"
                );
                let mut state = state.write().await;
                if *state != GatewayState::Draining {
                    *state = GatewayState::Armed;
                }
            }
            result
        }))
    } else {
        *gateway.write().await = GatewayState::Unconfigured;
        info!(
            missing = gateway_prerequisites(&config).unwrap_err(),
            status = ?ComponentStatus::Down,
            "gateway prerequisites missing; gateway parked, /readyz reports down"
        );
        None
    };

    let http = serve(&config.listen_addr, state, shutdown.clone());
    let result = match gateway_task {
        Some(task) => supervise_gateway(task, http, gateway, shutdown).await,
        None => http.await,
    };
    if let Err(err) = result {
        tracing::error!(error = %err, "container service failed");
        std::process::exit(1);
    }
}

/// `--help` covers both the gateway server and the backup CLI.
async fn print_backup_help_and_exit() -> ! {
    let code = backup_cli::dispatch(&["--help".to_owned()]).await;
    std::process::exit(code);
}

/// Validate before spawning: missing bindings park the gateway, whereas a
/// configured task's failures remain fatal. Errors contain binding names only.
fn gateway_prerequisites(config: &Config) -> Result<(&str, &str, u64), &'static str> {
    let token = config
        .discord_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .ok_or("DISCORD_TOKEN")?;
    let url = config
        .database_url
        .as_deref()
        .filter(|url| !url.is_empty())
        .ok_or("DATABASE_URL")?;
    let guild_id = config.guild_id.filter(|id| *id != 0).ok_or("GUILD_ID")?;
    Ok((token, url, guild_id))
}

/// A configured gateway is essential: never leave a health-only zombie after
/// initialization/dispatch failure, stream termination, or a task panic. Exit
/// nonzero so the Container supervisor can restart from the committed checkpoint.
/// Source: https://docs.rs/tokio/1/tokio/macro.select.html#cancellation-safety
async fn supervise_gateway(
    task: tokio::task::JoinHandle<Result<(), sqlx::Error>>,
    http: impl std::future::Future<Output = std::io::Result<()>>,
    state: Arc<RwLock<GatewayState>>,
    shutdown: tokio::sync::watch::Sender<bool>,
) -> std::io::Result<()> {
    supervise_gateway_bounded(
        task,
        http,
        state,
        shutdown,
        dispatch::DISPATCH_DRAIN_MAX + std::time::Duration::from_secs(5),
    )
    .await
}

async fn supervise_gateway_bounded(
    mut task: tokio::task::JoinHandle<Result<(), sqlx::Error>>,
    http: impl std::future::Future<Output = std::io::Result<()>>,
    state: Arc<RwLock<GatewayState>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    shutdown_max: std::time::Duration,
) -> std::io::Result<()> {
    let mut stopping = shutdown.subscribe();
    futures_util::pin_mut!(http);
    let http_result = tokio::select! {
        biased;
        _ = stopping.wait_for(|stopping| *stopping) => None,
        result = &mut http => Some(result),
        // Never expose task/SQL errors: they may contain connection secrets.
        _ = &mut task => return Err(std::io::Error::other(
            "gateway task stopped; container restart required",
        )),
    };
    *state.write().await = GatewayState::Draining;
    shutdown.send_replace(true);
    // Do not abort/drop the gateway future: it supervises a non-cancellable
    // blocking writer. Signal reception to stop and retain its bounded drain.
    // Any deadline/failure returns Err, and main exits immediately rather than
    // waiting indefinitely for Tokio to shut down a detached blocking writer.
    tokio::time::timeout(shutdown_max, async {
        match task.await {
            Ok(Ok(())) => {}
            _ => {
                return Err(std::io::Error::other(
                    "gateway drain failed; restart required",
                ))
            }
        }
        match http_result {
            Some(result) => result,
            None => http.await,
        }
    })
    .await
    .unwrap_or_else(|_| Err(std::io::Error::other("service shutdown deadline exceeded")))
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
