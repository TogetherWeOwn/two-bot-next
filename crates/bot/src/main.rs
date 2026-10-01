//! two-bot-next container entrypoint.
//!
//! Serves liveness (`/health`) and readiness (`/readyz`) and runs the gateway
//! shard supervisor (S3). Without `DISCORD_TOKEN`, `DATABASE_URL`, or a nonzero
//! `GUILD_ID` the shard stays parked and `/readyz` reports `gateway: down`
//! (HTTP 503) — the Container boots healthy on incomplete staging config.

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
mod website_jobs;

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::info;
use two_bot_core::{ComponentStatus, Config, VoiceGates};

use gateway::{
    build_pipeline, build_shard, build_voice_runtime, ensure_crypto_provider, intents_from_env,
    run_shard, GatewayState,
};
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

    let state = Arc::new(RwLock::new(GatewayState::new(&config)));
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

    // V1 voice rooms: per-guild lifecycle actors fed by the gateway sink.
    // Inert unless TWO_VOICE=1 with token + database present; any failure
    // degrades to voice-off with a warn, never a boot failure.
    let voice = build_voice_runtime(&config, VoiceGates::from_env().enabled).await;

    let gateway_task = match gateway_prerequisites(&config) {
        Ok((token, url, guild_id)) => {
            let token = token.to_owned();
            let url = url.to_owned();
            let state = Arc::clone(&state);
            let voice = voice.clone();
            Some(tokio::spawn(async move {
                let result: Result<(), sqlx::Error> = async {
                    // Runtime is DML-only; the operator migrates before startup.
                    let db =
                        two_bot_cutover::connect(&url, two_bot_cutover::DB_POOL_MAX_DEFAULT, true)
                            .await?;
                    metrics_http::register_pool(db.pool().clone());
                    let store = two_bot_cutover::gateway_session::GatewaySessionStore::new(
                        db.pool().clone(),
                        guild_id.to_string(),
                        0,
                    );
                    let saved = gateway::load_boot_session(&store).await?;
                    let pipeline = Arc::new(build_pipeline(store.milestones().await?));
                    // Shared command runtime (TOG-11020; S4 sticky slice was
                    // TOG-10309): ONE router + REST executor + sqlx stores
                    // over the same pool. `None` on bad env gates — the shard
                    // still boots without the command surface.
                    let runtime = command_runtime::CommandRuntime::from_env(
                        db.pool().clone(),
                        &token,
                        guild_id,
                    );
                    let shard = build_shard(
                        token,
                        intents_from_env(),
                        saved.as_ref(),
                        gateway_url.as_deref(),
                    );
                    info!(
                        resume = saved.is_some(),
                        "durable gateway initialized; shard connecting"
                    );
                    run_shard(shard, pipeline, Arc::clone(&state), store, runtime, voice).await
                }
                .await;
                if result.is_err() {
                    // Do not print sqlx errors: configuration errors may contain a URL.
                    tracing::error!(
                        "durable gateway failed; checkpoint unchanged, readiness unavailable"
                    );
                    *state.write().await = GatewayState::Armed;
                }
                result
            }))
        }
        Err(missing) => {
            *state.write().await = GatewayState::Unconfigured;
            info!(
                missing,
                status = ?ComponentStatus::Down,
                "gateway prerequisites missing; gateway parked, /readyz reports down"
            );
            None
        }
    };

    let (shutdown, _) = tokio::sync::watch::channel(false);
    let http = serve(&config, listener, state, shutdown.clone());
    let result = match gateway_task {
        Some(task) => supervise_gateway(task, http, shutdown).await,
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
/// Source: https://docs.rs/tokio/1/tokio/macro.select.html#cancellation-safety
async fn supervise_gateway(
    mut task: tokio::task::JoinHandle<Result<(), sqlx::Error>>,
    http: impl std::future::Future<Output = std::io::Result<()>>,
    shutdown: tokio::sync::watch::Sender<bool>,
) -> std::io::Result<()> {
    tokio::pin!(http);
    tokio::select! {
        biased;
        // Never expose task/SQL errors: they may contain connection secrets.
        _ = &mut task => {
            // Sticky even if HTTP has not subscribed yet. Keep polling HTTP so
            // its job supervisor can cancel and join every active action.
            shutdown.send_replace(true);
            let _ = http.await;
            Err(std::io::Error::other(
                "gateway task stopped; container restart required",
            ))
        },
        result = &mut http => {
            task.abort();
            let _ = task.await;
            result
        }
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
