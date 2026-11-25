use super::*;
use std::{collections::HashMap, time::Duration};
use two_bot_core::{backup::guild_config::TWO_STAGING_GUILD_ID, Secret};

const CA: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/event_write_cutoff/fixtures/recorder-ca.der"
);
const SIGNING: &str = "rehearsal:synthetic-efefefefefefefefefefefefefefefefefefefefefefefef";

fn config() -> Config {
    Config {
        discord_token: Some(Secret::new(format!("synthetic-{}", "ab".repeat(24)))),
        database_url: Some(Secret::new(format!(
            "postgres://postgres:synthetic-{}@127.0.0.1:5432/event_cutoff?sslmode=disable",
            "cd".repeat(24)
        ))),
        listen_addr: "127.0.0.1:8080".into(),
        guild_id: Some(TWO_STAGING_GUILD_ID.parse().unwrap()),
    }
}

fn receiver(bind: String) -> InternalActionConfig {
    let channel = "ann:111111111111111114";
    InternalActionConfig::from_map(&HashMap::from([
        ("TWO_INTERNAL_ACTIONS".into(), "1".into()),
        ("TWO_INTERNAL_BIND".into(), bind),
        ("TWO_INTERNAL_KEYS".into(), SIGNING.into()),
        ("TWO_INTERNAL_CALLERS".into(), "rehearsal:fixture".into()),
        ("TWO_INTERNAL_CHANNEL_KEYS".into(), channel.into()),
    ]))
    .unwrap()
    .unwrap()
}

#[test]
fn fixture_rejects_external_network_and_non_fixture_database() {
    let lo = "header\nheader\nlo: 0\n";
    let status = "CapPrm:\t0000000000000000\nCapEff:\t0000000000000000\n\
                  CapBnd:\t0000000000000000\nCapAmb:\t0000000000000000\nNoNewPrivs:\t1";
    assert!(isolated(lo, status));
    assert!(!isolated(&format!("{lo}eth0: 0\n"), status));
    assert!(!isolated(lo, &status.replace("0000\n", "0001\n")));
    let mut cfg = config();
    let bind = receiver("127.0.0.1:8091".into());
    assert!(verify_config(&cfg, &bind, SIGNING, []).is_ok());
    let url = cfg.database_url.as_ref().unwrap().expose();
    cfg.database_url = Some(Secret::new(format!("{url}&host=192.0.2.1")));
    assert_eq!(
        verify_config(&cfg, &bind, SIGNING, []),
        Err("rehearsal_database_invalid")
    );
}

#[test]
fn fixture_rejects_ordinary_credentials_and_non_loopback_receiver() {
    let bind = receiver("127.0.0.1:8091".into());
    let mut cfg = config();
    let wrong_bind = receiver("10.0.0.1:8091".into());
    let error = verify_config(&cfg, &wrong_bind, SIGNING, []).unwrap_err();
    assert_eq!(error, "rehearsal_receiver_not_loopback");
    for variable in ["PGHOSTADDR", "TWO_DEPLOY_ENV"] {
        let error = verify_config(&cfg, &bind, SIGNING, [variable.into()]).unwrap_err();
        assert_eq!(error, "rehearsal_credentials_invalid");
    }
    let error = verify_config(&cfg, &bind, "rehearsal:ordinary", []).unwrap_err();
    assert_eq!(error, "rehearsal_credentials_invalid");
    cfg.discord_token = Some(Secret::new("ordinary.fixture.token".into()));
    let error = verify_config(&cfg, &bind, SIGNING, []).unwrap_err();
    assert_eq!(error, "rehearsal_token_not_synthetic");
}

#[tokio::test]
async fn verified_fixture_binds_and_drains_the_shared_receiver() {
    crate::gateway::ensure_crypto_provider();
    let root = Path::new(CA);
    assert!(fixture_connector(root.parent().unwrap()).is_none());
    let oversized = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    assert!(fixture_connector(&oversized).is_none());
    let connector = fixture_connector(root).unwrap();
    let cfg = config();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let bind = receiver(socket.local_addr().unwrap().to_string());
    let (token, _) = verify_config(&cfg, &bind, SIGNING, []).unwrap();
    let pool = two_bot_store::pool::connect_lazy_pool_with_tls(
        "postgres://agent_test@agent-testdb:5432/internal_http_unit?sslmode=disable",
        TlsPolicy::LocalOnly,
        1,
    )
    .unwrap();
    let (_stop, stopping) = watch::channel(true);
    drop(socket);
    tokio::time::timeout(
        Duration::from_secs(5),
        serve_receiver(&cfg, bind, token, pool, connector, stopping),
    )
    .await
    .unwrap()
    .unwrap();
}
