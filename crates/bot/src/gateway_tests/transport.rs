use super::*;

/// Construct a test shard with literal-loopback proxy and override endpoints.
pub(super) fn test_shard(
    saved: Option<&GatewaySession>,
    proxy: &str,
    resume_override: Option<&str>,
) -> Result<Shard, &'static str> {
    if !crate::gateway::is_loopback_gateway(proxy) {
        return Err("proxy must be a literal loopback socket");
    }
    if let Some(url) = resume_override {
        if saved.is_none() || !crate::gateway::is_loopback_gateway(url) {
            return Err("resume override requires a session and a literal loopback socket");
        }
    }
    let config = crate::gateway::build_shard_config(TOKEN.into(), Intents::empty(), saved);
    let mut builder = ConfigBuilder::from(config).proxy_url(proxy.to_owned());
    if let Some(url) = resume_override {
        builder = builder.resume_url(url.to_owned());
    }
    Ok(Shard::with_config(ShardId::ONE, builder.build()))
}

#[test]
fn saved_session_uses_its_resume_url_by_default() {
    let saved = checkpoint("session", 42, "ws://127.0.0.1:1234");
    let shard = test_shard(Some(&saved), "ws://127.0.0.1:2345", None).unwrap();
    assert_eq!(shard.resume_url(), Some(saved.resume_url.as_str()));
    let session = shard.session().unwrap();
    assert_eq!(session.id(), "session");
    assert_eq!(session.sequence(), 42);
}

#[test]
fn explicit_resume_override_is_distinct_from_saved_url() {
    let saved = checkpoint("session", 42, "ws://127.0.0.1:1234");
    let shard = test_shard(
        Some(&saved),
        "ws://127.0.0.1:2345",
        Some("ws://127.0.0.1:3456"),
    )
    .unwrap();
    assert_eq!(shard.resume_url(), Some("ws://127.0.0.1:3456"));
    assert_eq!(shard.session().unwrap().id(), "session");
}

#[test]
fn no_session_has_no_resume_and_cannot_override() {
    let shard = test_shard(None, "ws://127.0.0.1:2345", None).unwrap();
    assert!(shard.session().is_none());
    assert!(shard.resume_url().is_none());
    assert!(test_shard(None, "ws://127.0.0.1:2345", Some("ws://127.0.0.1:3456")).is_err());
}

#[test]
fn rejects_non_loopback_proxy_and_resume_override() {
    let saved = checkpoint("session", 42, "ws://127.0.0.1:1234");
    assert!(test_shard(Some(&saved), "wss://gateway.discord.gg", None).is_err());
    assert!(test_shard(
        Some(&saved),
        "ws://127.0.0.1:2345",
        Some("wss://gateway.discord.gg")
    )
    .is_err());
}
