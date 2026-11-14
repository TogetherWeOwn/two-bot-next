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
    let config = crate::gateway::build_shard_config_with_policy(
        TOKEN.into(),
        Intents::empty(),
        saved,
        two_bot_core::gateway_session::ResumePolicy::for_bootstrap(Some(proxy)),
    );
    let mut builder = ConfigBuilder::from(config).proxy_url(proxy.to_owned());
    if let Some(url) = resume_override {
        builder = builder.resume_url(url.to_owned());
    }
    Ok(Shard::with_config(ShardId::ONE, builder.build()))
}

#[tokio::test]
async fn saved_session_uses_its_resume_url_by_default() {
    let saved = checkpoint("session", 42, "ws://127.0.0.1:1234");
    let shard = test_shard(Some(&saved), "ws://127.0.0.1:2345", None).unwrap();
    assert_eq!(shard.resume_url(), Some(saved.resume_url.as_str()));
    let session = shard.session().unwrap();
    assert_eq!(session.id(), "session");
    assert_eq!(session.sequence(), 42);
}

#[tokio::test]
async fn explicit_resume_override_is_distinct_from_saved_url() {
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

#[tokio::test]
async fn no_session_has_no_resume_and_cannot_override() {
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

/// Assert a shard installed exactly the saved session and URL, or neither.
fn assert_installed(shard: &Shard, url: &str, installed: bool) {
    assert_eq!(shard.resume_url(), installed.then_some(url));
    assert_eq!(
        shard.session().map(|s| (s.id(), s.sequence())),
        installed.then_some(("session", 42))
    );
}

#[tokio::test]
async fn strict_and_checked_builders_and_received_fallback() {
    use crate::gateway::{
        build_shard, build_shard_config, build_shard_config_with_policy, checkpoint_resume_url,
    };
    use two_bot_core::gateway_session::{
        boot_action, boot_action_with_policy, BootAction, BootDirective, ResumePolicy,
    };
    let resume = |ok| {
        if ok {
            BootAction::Resume
        } else {
            BootAction::DiscardAndIdentify
        }
    };
    let local = Some("ws://127.0.0.1:2345");
    let bootstraps = [
        None,
        local,
        Some("ws://localhost:2345"),
        Some("wss://remote.invalid"),
    ];
    let urls = [
        RESUME_URL,
        "ws://127.0.0.1:1234",
        "ws://[::1]:1234",
        "wss://remote.invalid",
        "ws://localhost:1234",
        "",
    ];
    for bootstrap in bootstraps {
        let policy = ResumePolicy::for_bootstrap(bootstrap);
        for url in urls {
            let saved = checkpoint("session", 42, url);
            let strict_ok = url == RESUME_URL;
            let ok = strict_ok || (bootstrap == local && crate::gateway::is_loopback_gateway(url));
            let at = saved.updated_at_ms;
            assert_eq!(boot_action(Some(&saved), at), resume(strict_ok));
            let directive = BootDirective::None;
            assert_eq!(
                boot_action_with_policy(Some(&saved), directive, at, policy),
                resume(ok)
            );
            let config = build_shard_config_with_policy(
                TOKEN.into(),
                Intents::empty(),
                Some(&saved),
                policy,
            );
            assert_installed(&Shard::with_config(ShardId::ONE, config), url, ok);
            let built = build_shard(TOKEN.into(), Intents::empty(), Some(&saved), bootstrap);
            assert_installed(&built, url, ok);
            let config = build_shard_config(TOKEN.into(), Intents::empty(), Some(&saved));
            assert_installed(&Shard::with_config(ShardId::ONE, config), url, strict_ok);
            let normalized = Some(policy.normalize(url));
            let fallback = |current, id| checkpoint_resume_url(policy, current, Some(&saved), id);
            assert_eq!(
                checkpoint_resume_url(policy, Some(url), None, "session"),
                normalized
            );
            assert_eq!(fallback(None, "session"), normalized);
            assert_eq!(fallback(None, "new-session"), None);
        }
    }
}

#[tokio::test]
async fn public_builders_preserve_valid_discord_session_and_empty_state() {
    let saved = checkpoint("session", 42, RESUME_URL);
    for saved in [None, Some(&saved)] {
        let config = crate::gateway::build_shard_config(TOKEN.into(), Intents::empty(), saved);
        assert_installed(
            &Shard::with_config(ShardId::ONE, config),
            RESUME_URL,
            saved.is_some(),
        );
        for bootstrap in [None, Some("ws://127.0.0.1:2345")] {
            let shard =
                crate::gateway::build_shard(TOKEN.into(), Intents::empty(), saved, bootstrap);
            assert_installed(&shard, RESUME_URL, saved.is_some());
        }
    }
}
