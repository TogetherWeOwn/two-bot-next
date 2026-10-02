//! two-bot-next container entrypoint.
//!
//! Serves liveness (`/health`) and readiness (`/readyz`) and runs the gateway
//! shard supervisor (S3). Without `DISCORD_TOKEN`, `DATABASE_URL`, or a nonzero
//! `GUILD_ID` the shard stays parked and `/readyz` reports `gateway: down`
//! (HTTP 503) — the Container boots healthy on incomplete staging config.

mod activation;
mod audit_runtime;
mod backup_cli;
mod command_runtime;
#[cfg(test)]
mod command_runtime_tests;
mod community_jobs;
mod database_roles_cli;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../discord/tests/common/mod.rs"]
mod discord_test_common;
mod dispatch;
mod erasure_cli;
mod gateway;
mod gateway_metrics;
#[cfg(test)]
mod gateway_tests;
mod jobs;
#[cfg(test)]
mod lifecycle_tests;
mod metrics_http;
mod preflight;
mod server;
mod ticket_runtime;
mod website_jobs;

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::info;
use two_bot_core::{ComponentStatus, Config};

use gateway::{
    build_persistent_pipeline, build_shard, ensure_crypto_provider, intents_from_env, run_shard,
    GatewayState,
};
use server::SharedState;
use website_jobs::serve;

#[tokio::main]
async fn main() {
    ensure_crypto_provider();
    let cli_args: Vec<String> = std::env::args().skip(1).collect();
    if cli_args.first().is_some_and(|arg| arg == "preflight") {
        std::process::exit(preflight::dispatch(&cli_args[1..]).await);
    }
    // Docker HEALTHCHECK probe: GET /health on the configured port and exit
    // 0/1. Kept dependency-free (std + tokio only) so the check path cannot
    // rot behind an HTTP-client upgrade.
    if std::env::args().any(|arg| arg == "--healthcheck") {
        std::process::exit(healthcheck().await);
    }

    // Operator CLI (TOG-9881): backup/restore + sealed guild-config snapshot.
    // No subcommand falls through to the gateway path below. sqlx is linked
    // (core `db` feature) so these paths can open Postgres directly.
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

    // Evaluate all five capabilities once, before any handler registration.
    // Refusals narrow the command surface, not liveness or analytics jobs.
    let activation = activation::BootActivation::from_config(&config);
    activation.log_refusals();
    let gateway = Arc::new(RwLock::new(GatewayState::new(&config)));
    let listener = server::bind(&config.listen_addr)
        .await
        .unwrap_or_else(|err| {
            tracing::error!(error = %err, "container listener failed");
            std::process::exit(1);
        });
    let gateway_url = match std::env::var("DISCORD_GATEWAY_URL") {
        Ok(url) => Some(url),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            tracing::error!("DISCORD_GATEWAY_URL must be valid UTF-8");
            std::process::exit(1);
        }
    };
    if gateway_url
        .as_deref()
        .is_some_and(|url| !gateway::is_loopback_gateway(url))
    {
        tracing::error!("DISCORD_GATEWAY_URL must be a loopback mock websocket address");
        std::process::exit(1);
    }

    let store = match gateway_prerequisites(&config).ok().map(|(_, url, _)| url) {
        Some(url) => match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            // Runtime is DML-only; the operator applies both store and gateway
            // migrations and the web contract before startup.
            two_bot_store::Store::connect(url, true),
        )
        .await
        {
            Ok(Ok(store)) => {
                metrics_http::register_pool(store.pool().clone());
                Some(store)
            }
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

    let (shutdown, stopping) = tokio::sync::watch::channel(false);
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
                let store = two_bot_cutover::gateway_session::GatewaySessionStore::new(
                    pool.clone(),
                    guild_id.to_string(),
                    0,
                );
                let saved = gateway::load_boot_session(&store).await?;
                let pipeline =
                    Arc::new(build_persistent_pipeline(&store, guild_id, token.clone()).await?);
                // ONE router + REST executor + sqlx stores over the same pool.
                // Bad command env gates still park only the command surface.
                let runtime =
                    command_runtime::CommandRuntime::from_env(pool, &token, guild_id, &activation);
                let shard = build_shard(
                    token,
                    intents_from_env(&activation),
                    saved.as_ref(),
                    gateway_url.as_deref(),
                );
                info!(
                    resume = saved.is_some(),
                    "durable gateway initialized; shard connecting"
                );
                run_shard(
                    shard,
                    pipeline,
                    Arc::clone(&state),
                    store,
                    runtime,
                    async move {
                        server::shutdown_requested(stopping).await;
                    },
                )
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

    let http = serve(&config, listener, state, shutdown.clone());
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
    println!("{}", preflight::USAGE);
    print!("{}", erasure_cli::USAGE);
    let code = backup_cli::dispatch(&["--help".to_owned()]).await;
    std::process::exit(code);
}

/// Validate before spawning: missing bindings park the gateway, whereas a
/// configured task's failures remain fatal. Errors contain binding names only.
fn gateway_prerequisites(config: &Config) -> Result<(&str, &str, u64), &'static str> {
    let token = config
        .discord_token
        .as_ref()
        .map(|secret| secret.expose().as_str())
        .filter(|token| !token.is_empty())
        .ok_or("DISCORD_TOKEN")?;
    let url = config
        .database_url
        .as_ref()
        .map(|secret| secret.expose().as_str())
        .filter(|url| !url.is_empty())
        .ok_or("DATABASE_URL")?;
    let guild_id = config.guild_id.filter(|id| *id != 0).ok_or("GUILD_ID")?;
    Ok((token, url, guild_id))
}

/// A configured gateway is essential: never leave a health-only zombie after
/// initialization/dispatch failure, stream termination, or a task panic. Exit
/// nonzero so the Container supervisor can restart from the committed checkpoint.
/// Source: <https://docs.rs/tokio/1/tokio/macro.select.html#cancellation-safety>
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
    let stopping = shutdown.subscribe();
    futures_util::pin_mut!(http);
    let (http_result, gateway_stopped) = tokio::select! {
        biased;
        _ = server::shutdown_requested(stopping) => (None, false),
        // Check the essential task before the first HTTP poll: cancellation
        // must be sticky even if the HTTP/job owner has not subscribed yet.
        // Never expose task/SQL errors: they may contain connection secrets.
        _ = &mut task => (None, true),
        result = &mut http => (Some(result), false),
    };
    *state.write().await = GatewayState::Draining;
    shutdown.send_replace(true);
    // Retain the gateway JoinHandle through HTTP/job cleanup, never abort its
    // non-cancellable blocking writer. Poll both drains so neither failure can
    // skip the other's cleanup. A fatal deadline returns Err; main exits rather
    // than waiting for Tokio to shut down a detached blocking writer.
    tokio::time::timeout(shutdown_max, async {
        let drain_http = async {
            match http_result {
                Some(result) => result,
                None => http.await,
            }
        };
        if gateway_stopped {
            // The handle was already consumed by select; do not poll it twice.
            let _ = drain_http.await;
            return Err(std::io::Error::other(
                "gateway task stopped; container restart required",
            ));
        }
        let (gateway_result, http_result) = tokio::join!(&mut task, drain_http);
        match gateway_result {
            Ok(Ok(())) => http_result,
            _ => Err(std::io::Error::other(
                "gateway drain failed; restart required",
            )),
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
