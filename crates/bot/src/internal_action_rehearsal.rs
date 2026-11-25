//! Receiver-only entry point for the disposable installed-binary fixture.

use std::{collections::HashMap, io::Read, sync::Arc};

use tokio::sync::{watch, RwLock};
use two_bot_core::{database_tls::TlsPolicy, internal_action_config::InternalActionConfig, Config};
use two_bot_discord::internal_actions::RehearsalAnnouncementConnector;

use crate::internal_action_http::{self, ReceiverMode};

const CA_PATH: &str = "/fixture/announcement-ca.der";
const MAX_CA_BYTES: u64 = 16 * 1024;

pub(crate) async fn dispatch(args: &[String]) -> i32 {
    match serve(args).await {
        Ok(()) => 0,
        Err(error_class) => {
            tracing::error!(error_class, "internal-action rehearsal refused");
            1
        }
    }
}

async fn serve(args: &[String]) -> Result<(), &'static str> {
    if !args.is_empty() {
        return Err("rehearsal_arguments_invalid");
    }
    let interfaces = std::fs::read_to_string("/proc/self/net/dev")
        .map_err(|_| "rehearsal_network_unisolated")?;
    let status =
        std::fs::read_to_string("/proc/self/status").map_err(|_| "rehearsal_network_unisolated")?;
    if !is_isolated(&interfaces, &status) {
        return Err("rehearsal_network_unisolated");
    }
    let config = Config::from_env().map_err(|_| "rehearsal_config_invalid")?;
    let receiver = InternalActionConfig::from_env()
        .ok()
        .flatten()
        .ok_or("rehearsal_receiver_invalid")?;
    let (token, database) = validate(&config, &receiver, &std::env::vars().collect())?;
    let ca = fixture_root().ok_or("rehearsal_ca_invalid")?;
    let fixture = RehearsalAnnouncementConnector::new(ca).map_err(|_| "rehearsal_ca_invalid")?;
    let pool = two_bot_store::connect_pool_with_tls(database, TlsPolicy::LocalOnly)
        .await
        .map_err(|_| "rehearsal_database_connect_failed")?;
    let receiver = internal_action_http::bind(
        receiver,
        pool,
        token,
        crate::voice_snapshot::new_shared_cache(),
        crate::voice_snapshot::VoiceOccupancyClock::new(),
        Arc::new(RwLock::new(crate::gateway::GatewayState::new(&config))),
        ReceiverMode::Rehearsal(fixture),
    )
    .await
    .map_err(|_| "rehearsal_receiver_bind_failed")?;
    let (stop, stopping) = watch::channel(false);
    let signal = tokio::spawn(async move {
        crate::server::shutdown_signal().await;
        stop.send_replace(true);
    });
    let result = receiver.serve(stopping).await;
    signal.abort();
    result.map_err(|_| "rehearsal_receiver_failed")
}

fn is_isolated(interfaces: &str, status: &str) -> bool {
    // The runner creates a private namespace with no published ports. Also
    // check the process's view and reject a launch able to regain privileges.
    interfaces
        .lines()
        .skip(2)
        .map(|line| line.split_once(':').map(|(name, _)| name.trim()))
        .eq([Some("lo")])
        && ["CapPrm", "CapEff", "CapBnd", "CapAmb"]
            .into_iter()
            .all(|cap| {
                status
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .any(|(name, value)| name == cap && value.trim() == "0000000000000000")
            })
        && status.lines().any(|line| line == "NoNewPrivs:\t1")
}

fn validate<'a>(
    config: &'a Config,
    receiver: &InternalActionConfig,
    env: &HashMap<String, String>,
) -> Result<(&'a str, &'a str), &'static str> {
    if !receiver.listen_addr().ip().is_loopback() {
        return Err("rehearsal_receiver_not_loopback");
    }
    let token =
        crate::internal_receiver_prerequisites(config).map_err(|_| "rehearsal_config_invalid")?;
    if !RehearsalAnnouncementConnector::is_synthetic_token(token) {
        return Err("rehearsal_token_not_synthetic");
    }
    let key = env
        .get("TWO_INTERNAL_KEYS")
        .and_then(|keys| keys.strip_prefix("rehearsal:"));
    if !key.is_some_and(|key| fixture_secret(key, 64))
        || receiver.caller_for("rehearsal") != Some("fixture")
        || env.keys().any(|name| {
            name.starts_with("PG")
                || matches!(
                    name.as_str(),
                    "TWO_DEPLOY_ENV" | "TWO_INTERNAL_CONTAINER" | "TWO_MODERATION_AUDIT_SECRET"
                )
        })
    {
        return Err("rehearsal_credentials_invalid");
    }
    let database = config
        .database_url
        .as_ref()
        .ok_or("rehearsal_database_invalid")?
        .expose();
    if !is_fixture_database(database) {
        return Err("rehearsal_database_invalid");
    }
    Ok((token, database))
}

fn is_fixture_database(url: &str) -> bool {
    // This exact grammar leaves no driver defaults or query overrides for
    // host, port, database, credentials, TLS or Unix sockets.
    let Some((password, name)) = url
        .strip_prefix("postgres://postgres:")
        .and_then(|rest| rest.strip_suffix("?sslmode=disable"))
        .and_then(|rest| rest.split_once("@127.0.0.1:5432/"))
    else {
        return false;
    };
    fixture_secret(password, 48)
        && (name == "event_cutoff"
            || name
                .strip_prefix("event_cutoff_restored_")
                .is_some_and(|n| {
                    n.as_bytes()
                        .first()
                        .is_some_and(|b| (b'1'..=b'9').contains(b))
                        && n.parse::<u32>().is_ok()
                }))
}

fn fixture_secret(raw: &str, len: usize) -> bool {
    raw.strip_prefix("synthetic-").is_some_and(|hex| {
        hex.len() == len
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn fixture_root() -> Option<Vec<u8>> {
    let path = std::path::Path::new(CA_PATH);
    if !path.metadata().ok()?.is_file() {
        return None;
    }
    let mut der = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_CA_BYTES + 1)
        .read_to_end(&mut der)
        .ok()?;
    (der.len() <= MAX_CA_BYTES as usize).then_some(der)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database(name: &str) -> String {
        format!(
            "postgres://postgres:synthetic-{}@127.0.0.1:5432/{name}?sslmode=disable",
            "ab".repeat(24)
        )
    }

    #[test]
    fn network_requires_only_loopback_and_no_reacquirable_privileges() {
        let interfaces = "header\nheader\n  lo: 0 0 0\n";
        let status = "Pid:\t1\nCapPrm:\t0000000000000000\nCapEff:\t0000000000000000\n\
                      CapBnd:\t0000000000000000\nCapAmb:\t0000000000000000\nNoNewPrivs:\t1\n";
        assert!(is_isolated(interfaces, status));
        assert!(!is_isolated(&format!("{interfaces}eth0: 0 0 0\n"), status));
        assert!(!is_isolated(
            interfaces,
            &status.replace("0000\n", "0001\n")
        ));
        assert!(!is_isolated(
            interfaces,
            &status.replace("Privs:\t1", "Privs:\t0")
        ));
        assert!(!is_isolated("", status));
        assert!(!is_isolated(interfaces, ""));
    }

    #[test]
    fn database_requires_literal_loopback_and_fixture_identity() {
        let initial = database("event_cutoff");
        assert!(is_fixture_database(&initial));
        assert!(is_fixture_database(&database("event_cutoff_restored_12")));
        for invalid in [
            initial.replace("127.0.0.1", "localhost"),
            initial.replace("127.0.0.1", "192.0.2.1"),
            initial.replace(":5432", ""),
            initial.replace("synthetic-", ""),
            format!("{initial}&host=192.0.2.1"),
            database("ordinary"),
            database("event_cutoff_restored_0"),
            database("event_cutoff_restored_+1"),
            database("event_cutoff_restored_99999999999"),
        ] {
            assert!(!is_fixture_database(&invalid));
        }
    }

    #[test]
    fn startup_requires_the_synthetic_receiver_credentials() {
        let mut env = HashMap::from([
            ("TWO_INTERNAL_ACTIONS".into(), "1".into()),
            ("TWO_INTERNAL_BIND".into(), "127.0.0.1:8091".into()),
            (
                "TWO_INTERNAL_KEYS".into(),
                format!("rehearsal:synthetic-{}", "ab".repeat(32)),
            ),
            ("TWO_INTERNAL_CALLERS".into(), "rehearsal:fixture".into()),
            (
                "TWO_INTERNAL_CHANNEL_KEYS".into(),
                "ann:111111111111111114".into(),
            ),
        ]);
        let guild = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
        let mut config = Config {
            discord_token: Some(two_bot_core::Secret::new(format!(
                "synthetic-{}",
                "ab".repeat(24)
            ))),
            database_url: Some(two_bot_core::Secret::new(database("event_cutoff"))),
            listen_addr: String::new(),
            guild_id: Some(guild.parse().unwrap()),
        };
        let receiver = InternalActionConfig::from_map(&env).unwrap().unwrap();
        assert!(validate(&config, &receiver, &env).is_ok());
        env.insert("TWO_INTERNAL_BIND".into(), "10.0.0.1:8091".into());
        let wrong_bind = InternalActionConfig::from_map(&env).unwrap().unwrap();
        assert_eq!(
            validate(&config, &wrong_bind, &env),
            Err("rehearsal_receiver_not_loopback")
        );
        env.insert("PGOPTIONS".into(), "unexpected".into());
        assert_eq!(
            validate(&config, &receiver, &env),
            Err("rehearsal_credentials_invalid")
        );
        env.remove("PGOPTIONS");
        env.insert(
            "TWO_INTERNAL_KEYS".into(),
            format!("rehearsal:{}", "ab".repeat(32)),
        );
        assert_eq!(
            validate(&config, &receiver, &env),
            Err("rehearsal_credentials_invalid")
        );
        config.discord_token = Some(two_bot_core::Secret::new("ordinary.fixture.token".into()));
        assert_eq!(
            validate(&config, &receiver, &env),
            Err("rehearsal_token_not_synthetic")
        );
    }
}
