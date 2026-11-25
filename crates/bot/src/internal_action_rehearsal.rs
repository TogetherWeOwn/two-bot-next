//! Receiver-only entry point for the disposable installed-binary fixture.

use crate::internal_action_http::{self, ReceiverMode};
use std::{ffi::OsString, fs, future::Future, io::Read, path::Path, sync::Arc};
use tokio::sync::{watch, RwLock};
use two_bot_core::{database_tls::TlsPolicy, internal_action_config::InternalActionConfig, Config};
use two_bot_discord::internal_actions::RehearsalAnnouncementConnector as Connector;

pub(crate) async fn serve(args: &[String]) -> Result<(), &'static str> {
    if !args.is_empty() {
        return Err("rehearsal_arguments_invalid");
    }
    let network = "rehearsal_network_unisolated";
    let interfaces = fs::read_to_string("/proc/self/net/dev").map_err(|_| network)?;
    let status = fs::read_to_string("/proc/self/status").map_err(|_| network)?;
    let ready = isolated(&interfaces, &status);
    ready.then_some(()).ok_or(network)?;
    let config = Config::from_env().map_err(|_| "rehearsal_config_invalid")?;
    let receiver = InternalActionConfig::from_env()
        .map_err(|_| "rehearsal_receiver_invalid")?
        .ok_or("rehearsal_receiver_invalid")?;
    let (token, database, connector) = prepare_fixture(
        &config,
        &receiver,
        &std::env::var("TWO_INTERNAL_KEYS").unwrap_or_default(),
        std::env::vars_os().map(|(name, _)| name),
        Path::new("/fixture/announcement-ca.der"),
    )?;
    let pool = two_bot_store::connect_pool_with_tls(database, TlsPolicy::LocalOnly)
        .await
        .map_err(|_| "rehearsal_database_connect_failed")?;
    let signal = crate::server::shutdown_signal();
    serve_receiver(&config, receiver, token, pool, connector, signal)
        .await
        .map_err(|_| "rehearsal_receiver_failed")
}

fn prepare_fixture<'a>(
    config: &'a Config,
    receiver: &InternalActionConfig,
    signing: &str,
    env_names: impl IntoIterator<Item = OsString>,
    root: &Path,
) -> Result<(&'a str, &'a str, Connector), &'static str> {
    if !receiver.listen_addr().ip().is_loopback() {
        return Err("rehearsal_receiver_not_loopback");
    }
    let invalid = "rehearsal_config_invalid";
    let token = crate::internal_receiver_prerequisites(config).map_err(|_| invalid)?;
    if !Connector::is_synthetic_token(token) {
        return Err("rehearsal_token_not_synthetic");
    }
    if !signing
        .strip_prefix("rehearsal:")
        .is_some_and(Connector::is_synthetic_token)
        || receiver.caller_for("rehearsal") != Some("fixture")
        || env_names.into_iter().any(|name| {
            let name = name.to_string_lossy();
            name.starts_with("PG")
                || matches!(
                    name.as_ref(),
                    "TWO_DEPLOY_ENV" | "TWO_INTERNAL_CONTAINER" | "TWO_MODERATION_AUDIT_SECRET"
                )
        })
    {
        return Err("rehearsal_credentials_invalid");
    }
    let (_, database, _) = crate::gateway_prerequisites(config).map_err(|_| invalid)?;
    if !fixture_database(database) {
        return Err("rehearsal_database_invalid");
    }
    let connector = fixture_connector(root).ok_or("rehearsal_ca_invalid")?;
    Ok((token, database, connector))
}

async fn serve_receiver(
    config: &Config,
    receiver: InternalActionConfig,
    token: &str,
    pool: sqlx::PgPool,
    connector: Connector,
    signal: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let (stop, stopping) = watch::channel(false);
    tokio::spawn(async move {
        signal.await;
        stop.send_replace(true);
    });
    internal_action_http::bind(
        receiver,
        pool,
        token,
        crate::voice_snapshot::new_shared_cache(),
        crate::voice_snapshot::VoiceOccupancyClock::new(),
        Arc::new(RwLock::new(crate::gateway::GatewayState::new(config))),
        ReceiverMode::Rehearsal(connector),
    )
    .await?
    .serve(stopping)
    .await
}

fn isolated(interfaces: &str, status: &str) -> bool {
    interfaces
        .lines()
        .skip(2)
        .map(|line| line.split_once(':').map(|(name, _)| name.trim()))
        .eq([Some("lo")])
        && ["CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"]
            .into_iter()
            .all(|cap| {
                status
                    .lines()
                    .any(|line| line.strip_prefix(cap) == Some("\t0000000000000000"))
            })
        && status.lines().any(|line| line == "NoNewPrivs:\t1")
}

fn fixture_database(url: &str) -> bool {
    url.strip_prefix("postgres://postgres:")
        .and_then(|url| url.strip_suffix("@127.0.0.1:5432/event_cutoff?sslmode=disable"))
        .is_some_and(Connector::is_synthetic_token)
}

fn fixture_connector(path: &Path) -> Option<Connector> {
    const LIMIT: u64 = 16 * 1024;
    if !path.metadata().ok()?.is_file() {
        return None;
    }
    let mut der = Vec::new();
    let file = fs::File::open(path).ok()?;
    let len = file.take(LIMIT + 1).read_to_end(&mut der).ok()?;
    if len > LIMIT as usize {
        return None;
    }
    Connector::new(der).ok()
}

#[cfg(test)]
mod tests;
