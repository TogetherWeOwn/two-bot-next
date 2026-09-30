//! Synthetic credential regressions; no live service or environment access.
use std::fmt::Debug;
use two_bot_core::{
    backup::{
        guild_config_api::{checked_base, GuildConfigDiscordApi},
        http::{HttpError, HttpResponse},
        s3,
    },
    internal_actions::{parse_keys, KeyRing},
    mac::moderation_audit_secret,
    Config, Secret,
};

fn assert_redacted(value: &impl Debug, secrets: &[&str]) {
    for rendered in [format!("{value:?}"), format!("{value:#?}")] {
        for secret in secrets {
            assert!(
                !rendered.contains(secret),
                "synthetic credential reached Debug"
            );
        }
    }
}

#[test]
fn config_and_direct_fields_redact_entire_urls() {
    let token = "fixture-discord-credential";
    let url = "postgres://fixture-user:fixture-db-password@localhost/db?key=fixture-query";
    let config = Config {
        discord_token: Some(Secret::new(token.to_owned())),
        database_url: Some(Secret::new(url.to_owned())),
        listen_addr: "127.0.0.1:0".to_owned(),
        guild_id: Some(1),
    };
    let secrets = [
        token,
        url,
        "fixture-user",
        "fixture-db-password",
        "fixture-query",
    ];
    assert_redacted(&config, &secrets);
    assert_redacted(&config.discord_token, &secrets);
    assert_redacted(&config.database_url, &secrets);
    assert!(config.gateway_configured());
}

#[test]
fn s3_target_and_signed_request_hide_keys_and_authorization() {
    let target = s3::S3Target {
        endpoint: Secret::new(
            "https://fixture-user:fixture-password@s3.invalid/private".to_owned(),
        ),
        region: "auto".to_owned(),
        bucket: "fixture-backups".to_owned(),
        access_key_id: Secret::new("fixture-access-id".to_owned()),
        secret_access_key: Secret::new("fixture-s3-key-material".to_owned()),
        prefix: None,
    };
    let request = s3::sign_put(
        &target,
        "fixture.gz",
        b"synthetic",
        "20260930T000000Z",
        "20260930",
    );
    let auth = &request
        .headers
        .expose()
        .iter()
        .find(|(name, _)| name == "authorization")
        .unwrap()
        .1;
    let signature = auth.split("Signature=").nth(1).unwrap();
    let secrets = [
        "fixture-access-id",
        "fixture-s3-key-material",
        "fixture-user",
        "fixture-password",
        auth,
        signature,
    ];
    assert_redacted(&target, &secrets);
    assert_redacted(&request, &secrets);
    assert_redacted(&request.headers, &secrets);
    assert_redacted(&request.url, &secrets);
    assert!(auth.contains("Credential=fixture-access-id/"));
}

#[test]
fn invalid_s3_endpoint_never_echoes_its_credential() {
    for endpoint in [
        "invalid:fixture-endpoint-secret",
        "http://fixture-user:fixture-password@remote.invalid/path",
    ] {
        let vars = [
            ("TWO_BACKUP_S3_ENDPOINT", endpoint),
            ("TWO_BACKUP_S3_BUCKET", "fixture-backups"),
            ("TWO_BACKUP_S3_ACCESS_KEY_ID", "fixture-id"),
            ("TWO_BACKUP_S3_SECRET_ACCESS_KEY", "fixture-key"),
        ];
        let error = s3::load_s3_target(&|key| {
            vars.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        })
        .unwrap_err();
        assert_redacted(
            &error,
            &[endpoint, "fixture-endpoint-secret", "fixture-password"],
        );
        assert!(!error.to_string().contains(endpoint));
    }
}

#[test]
fn guild_config_client_and_invalid_base_hide_credentials() {
    let api = GuildConfigDiscordApi::new(
        None,
        None,
        "fixture-guild-config-token".to_owned(),
        "1".to_owned(),
        "2".to_owned(),
    )
    .unwrap();
    assert_redacted(&api, &["fixture-guild-config-token"]);
    assert_redacted(&api.token, &["fixture-guild-config-token"]);
    let error = checked_base(
        Some("https://fixture-user:fixture-password@remote.invalid"),
        "GUILD_CONFIG_API_BASE",
        "https://discord.com",
    )
    .unwrap_err();
    assert_redacted(&error, &["fixture-user", "fixture-password"]);
    assert!(!error.to_string().contains("fixture-user"));
}

#[test]
fn hmac_keys_and_moderation_loader_protect_direct_field_debug() {
    let marker = "fixture-internal-action-key-material-at-least-32-bytes";
    let keys = parse_keys(&format!("web:{marker}")).unwrap();
    assert_redacted(&keys, &[marker]);
    assert_eq!(format!("{:?}", keys[0].secret), "[REDACTED]");
    assert_redacted(&KeyRing::new(keys), &[marker]);
    let vars = [("TWO_MODERATION_AUDIT_SECRET".to_owned(), marker.to_owned())].into();
    let loaded = moderation_audit_secret(&vars, None);
    assert_redacted(&loaded, &[marker]);
    assert_eq!(loaded.unwrap().unwrap().expose(), marker);
}

#[test]
fn every_http_error_variant_redacts_url_and_echoed_credentials() {
    let url = "https://fixture-user:fixture-password@discord.invalid/api/webhooks/1/fixture-webhook?key=fixture-query";
    let echoed = "fixture-echoed-authorization";
    let errors = [
        HttpError::InvalidUrl {
            url: url.to_owned().into(),
            reason: echoed.to_owned().into(),
        },
        HttpError::ClearText {
            url: url.to_owned().into(),
        },
        HttpError::Transport {
            url: url.to_owned().into(),
            reason: echoed.to_owned().into(),
        },
        HttpError::Timeout {
            url: url.to_owned().into(),
            secs: 1,
        },
        HttpError::TooLarge {
            url: url.to_owned().into(),
            limit: 1,
        },
        HttpError::Status {
            url: url.to_owned().into(),
            status: http::StatusCode::FORBIDDEN,
            detail: echoed.to_owned().into(),
        },
    ];
    let secrets = [
        url,
        "fixture-user",
        "fixture-password",
        "fixture-webhook",
        "fixture-query",
        echoed,
    ];
    for error in errors {
        assert_redacted(&error, &secrets);
        for secret in secrets {
            assert!(!error.to_string().contains(secret));
        }
        assert!(std::error::Error::source(&error).is_none());
    }
}

#[test]
fn http_response_debug_and_detail_do_not_print_remote_echoes() {
    let marker = "fixture-response-echoed-secret";
    let response = HttpResponse {
        status: http::StatusCode::FORBIDDEN,
        body: marker.as_bytes().to_vec(),
        headers: vec![("authorization".to_owned(), marker.to_owned())],
    };
    assert_redacted(&response, &[marker]);
    assert_redacted(&response.detail(), &[marker]);
    assert_eq!(format!("{}", response.detail()), "[REDACTED]");
}

#[tokio::test]
async fn real_http_refusal_redacts_webhook_before_network_access() {
    let url = "http://remote.invalid/api/webhooks/1/fixture-webhook-token";
    let error = two_bot_core::backup::http::get(url, vec![], 1)
        .await
        .unwrap_err();
    assert_redacted(&error, &[url, "fixture-webhook-token"]);
    assert!(!error.to_string().contains("fixture-webhook-token"));
}
