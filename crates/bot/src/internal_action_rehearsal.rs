//! Receiver-only entry point for the disposable installed-binary fixture.

use std::{fs, io::Read, path::Path, sync::Arc};
use tokio::sync::{watch, RwLock};
use two_bot_core::{database_tls::TlsPolicy, internal_action_config::InternalActionConfig, Config};
use two_bot_discord::internal_actions::RehearsalAnnouncementConnector as Connector;

use crate::internal_action_http::{self, ReceiverMode};

pub(crate) async fn serve(args: &[String]) -> Result<(), &'static str> {
    if !args.is_empty() {
        return Err("rehearsal_arguments_invalid");
    }
    let network = "rehearsal_network_unisolated";
    let interfaces = fs::read_to_string("/proc/self/net/dev").map_err(|_| network)?;
    let status = fs::read_to_string("/proc/self/status").map_err(|_| network)?;
    if !isolated(&interfaces, &status) {
        return Err(network);
    }
    let invalid_config = "rehearsal_config_invalid";
    let config = Config::from_env().map_err(|_| invalid_config)?;
    let receiver = InternalActionConfig::from_env()
        .map_err(|_| "rehearsal_receiver_invalid")?
        .ok_or("rehearsal_receiver_invalid")?;
    if !receiver.listen_addr().ip().is_loopback() {
        return Err("rehearsal_receiver_not_loopback");
    }
    let token = crate::internal_receiver_prerequisites(&config).map_err(|_| invalid_config)?;
    if !Connector::is_synthetic_token(token) {
        return Err("rehearsal_token_not_synthetic");
    }
    let signing = std::env::var("TWO_INTERNAL_KEYS").unwrap_or_default();
    if !signing
        .strip_prefix("rehearsal:")
        .is_some_and(Connector::is_synthetic_token)
        || receiver.caller_for("rehearsal") != Some("fixture")
        || std::env::vars_os().any(|(name, _)| {
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
    let (_, database, _) = crate::gateway_prerequisites(&config).map_err(|_| invalid_config)?;
    if !fixture_database(database) {
        return Err("rehearsal_database_invalid");
    }
    let connector = fixture_connector().ok_or("rehearsal_ca_invalid")?;
    let pool = two_bot_store::connect_pool_with_tls(database, TlsPolicy::LocalOnly)
        .await
        .map_err(|_| "rehearsal_database_connect_failed")?;
    let bound = internal_action_http::bind(
        receiver,
        pool,
        token,
        crate::voice_snapshot::new_shared_cache(),
        crate::voice_snapshot::VoiceOccupancyClock::new(),
        Arc::new(RwLock::new(crate::gateway::GatewayState::new(&config))),
        ReceiverMode::Rehearsal(connector),
    )
    .await
    .map_err(|_| "rehearsal_receiver_bind_failed")?;
    let (stop, stopping) = watch::channel(false);
    tokio::spawn(async move {
        crate::server::shutdown_signal().await;
        stop.send_replace(true);
    });
    bound
        .serve(stopping)
        .await
        .map_err(|_| "rehearsal_receiver_failed")
}

fn isolated(interfaces: &str, status: &str) -> bool {
    // Inspect the actual namespace and refuse privileges that could change it.
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
    // Match the complete URL to rule out driver and query overrides.
    url.strip_prefix("postgres://postgres:")
        .and_then(|url| url.strip_suffix("@127.0.0.1:5432/event_cutoff?sslmode=disable"))
        .is_some_and(Connector::is_synthetic_token)
}

fn fixture_connector() -> Option<Connector> {
    const LIMIT: u64 = 16 * 1024;
    let path = Path::new("/fixture/announcement-ca.der");
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
#[test]
fn fixture_rejects_external_network_and_non_fixture_database() {
    let lo = "header\nheader\nlo: 0\n";
    let status = "CapPrm:\t0000000000000000\nCapEff:\t0000000000000000\n\
                      CapBnd:\t0000000000000000\nCapAmb:\t0000000000000000\nNoNewPrivs:\t1";
    assert!(isolated(lo, status));
    assert!(!isolated(&format!("{lo}eth0: 0\n"), status));
    assert!(!isolated(lo, &status.replace("0000\n", "0001\n")));
    let url = format!(
        "postgres://postgres:synthetic-{}@127.0.0.1:5432/event_cutoff?sslmode=disable",
        "ab".repeat(24)
    );
    assert!(fixture_database(&url));
    assert!(!fixture_database(&format!("{url}&host=192.0.2.1")));
}
