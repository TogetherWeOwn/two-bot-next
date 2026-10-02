//! Synthetic credential regressions; no live service or environment access.
use std::fmt::Debug;

#[path = "support/tracing_capture.rs"]
mod tracing_capture;
use two_bot_core::{
    backup::{
        guild_config_api::{checked_base, GuildConfigApiError, GuildConfigDiscordApi},
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

#[test]
fn accepted_guild_config_overrides_redact_private_paths() {
    let base = "http://localhost:9000/private/fixture-base-secret";
    let api = GuildConfigDiscordApi::new(
        Some(base),
        Some(base),
        "fixture-bot-token".to_owned(),
        "1".to_owned(),
        "2".to_owned(),
    )
    .unwrap();
    let secrets = [base, "fixture-base-secret", "fixture-bot-token"];
    assert_redacted(&api, &secrets);
    assert_redacted(&api.api_base, &secrets);
    assert_redacted(&api.cdn_base, &secrets);
}

#[test]
fn rejected_guild_config_overrides_redact_private_paths_and_queries() {
    let base = "http://localhost:9000/private/fixture-base-secret?key=fixture-query-secret";
    for (api_base, cdn_base) in [(Some(base), None), (None, Some(base))] {
        let error = GuildConfigDiscordApi::new(
            api_base,
            cdn_base,
            "fixture-bot-token".to_owned(),
            "1".to_owned(),
            "2".to_owned(),
        )
        .unwrap_err();
        let secrets = [
            base,
            "fixture-base-secret",
            "fixture-query-secret",
            "fixture-bot-token",
        ];
        assert_redacted(&error, &secrets);
        for secret in secrets {
            assert!(!error.to_string().contains(secret));
        }
        assert!(matches!(
            error,
            two_bot_core::backup::guild_config_api::GuildConfigApiError::BadBase(_)
        ));
    }
}

#[tokio::test]
async fn rejected_guild_write_does_not_echo_authorization_or_remote_json() {
    use axum::{extract::Request, http::StatusCode, routing::post, Json, Router};
    let app = Router::new().route(
        "/write",
        post(|request: Request| async move {
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "authorization": request.headers()["authorization"].to_str().unwrap(),
                    "message": "fixture-remote-json-secret"
                })),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut api = GuildConfigDiscordApi::new(
        Some(&base),
        None,
        "fixture-write-bot-secret".to_owned(),
        "1".to_owned(),
        "2".to_owned(),
    )
    .unwrap();
    let error = api
        .write("POST", "/write", serde_json::json!({}))
        .await
        .unwrap_err();
    server.abort();
    assert_eq!(api.writes, 0);
    assert_redacted(
        &error,
        &["fixture-write-bot-secret", "fixture-remote-json-secret"],
    );
    let shown = error.to_string();
    assert!(shown.contains("HTTP 403"));
    assert!(!shown.contains("fixture-write-bot-secret"));
    assert!(!shown.contains("fixture-remote-json-secret"));
}

#[tokio::test]
async fn emoji_non_image_content_type_never_echoes_remote_header() {
    use axum::{http::StatusCode, response::Response, routing::get, Router};
    let sentinel = "fixture-emoji-content-type-secret";
    let app = Router::new().route(
        "/emojis/e1.png",
        get(|| async {
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/x-fixture-emoji-content-type-secret")
                .body(axum::body::Body::from("ok"))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cdn_base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api = GuildConfigDiscordApi::new(
        None,
        Some(&cdn_base),
        "fixture-bot-token".to_owned(),
        "1".to_owned(),
        "2".to_owned(),
    )
    .unwrap();
    let emoji = serde_json::json!({
        "id": "e1",
        "name": "wave",
        "managed": false,
        "animated": false,
    });
    let emoji = emoji.as_object().unwrap();
    let error = api.capture_emoji_image(emoji).await.unwrap_err();
    server.abort();
    assert_redacted(&error, &[sentinel]);
    let shown = error.to_string();
    assert!(
        shown.contains("non-image"),
        "classification retained: {shown}"
    );
    assert!(!shown.contains(sentinel));
    assert!(!format!("{error:?}").contains(sentinel));
    // CLI diagnostics render the same error string (`eprintln!(... {err})`),
    // so the backup CLI cannot disclose the echoed header either.
    let cli = format!("guild-config-snapshot: capture failed: {error}");
    assert!(!cli.contains(sentinel));
}

#[test]
fn http_rejects_userinfo_before_hyper_can_log_it() {
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!(
                "http://fixture-uri-user:fixture-uri-password@{}/webhook",
                listener.local_addr().unwrap()
            );
            let error = two_bot_core::backup::http::get(&url, vec![], 1)
                .await
                .unwrap_err();
            assert!(matches!(error, HttpError::InvalidUrl { .. }));
            assert_redacted(&error, &["fixture-uri-user", "fixture-uri-password"]);
            // No request may reach the listener, not even one with userinfo stripped.
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), listener.accept())
                    .await
                    .is_err()
            );
            tracing::debug!("capture remains active");
        });
    });
    let text = capture.text();
    assert!(text.contains("capture remains active"));
    assert!(!text.contains("fixture-uri-user"));
    assert!(!text.contains("fixture-uri-password"));
}

#[test]
fn successful_http_debug_logs_hide_webhook_path_query_and_authorization() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!(
                "http://{}/api/webhooks/1/fixture-path-secret?key=fixture-query-secret",
                listener.local_addr().unwrap()
            );
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                }
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await
                    .unwrap();
                // Keep alive until the request future completes and returns to its pool.
                let _ = socket.read(&mut buf).await;
            });
            let response = two_bot_core::backup::http::get(
                &url,
                vec![(
                    "authorization".to_owned(),
                    "fixture-header-secret".to_owned(),
                )],
                1,
            )
            .await
            .unwrap();
            assert_eq!(response.status, http::StatusCode::OK);
            tokio::task::yield_now().await;
            server.abort();
            tracing::debug!("capture remains active");
        });
    });
    let text = capture.text();
    assert!(text.contains("capture remains active"));
    for secret in [
        "fixture-path-secret",
        "fixture-query-secret",
        "fixture-header-secret",
    ] {
        assert!(!text.contains(secret));
    }
}

#[cfg(feature = "db")]
#[test]
fn channel_store_rejects_unknown_query_secrets_without_sqlx_warning() {
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let error = two_bot_core::ChannelModerationStore::connect(
                "postgres://fixture-user:fixture-password@agent-testdb/db?api_key=fixture-query-secret&sslmode=invalid",
                1,
            )
            .await
            .unwrap_err();
            assert_redacted(&error, &["fixture-user", "fixture-password", "fixture-query-secret"]);
            assert!(!error.to_string().contains("fixture-query-secret"));
            tracing::warn!("capture remains active");
        });
    });
    let text = capture.text();
    assert!(text.contains("capture remains active"));
    assert!(!text.contains("fixture-query-secret"));
    assert!(!text.contains("ignoring unrecognized connect parameter"));
}

#[test]
fn guild_config_wrapper_rerender_keeps_webhook_userinfo_and_query_redacted() {
    // `GuildConfigApiError::Http` re-renders the wrapped error through
    // `http: {0}`: pin that the `#[from]` composition cannot reintroduce a
    // webhook token, userinfo, or query credential into Display or Debug.
    let url = "https://fixture-user:fixture-password@discord.invalid/api/webhooks/1/fixture-webhook-token?key=fixture-query-secret";
    let http = HttpError::Status {
        url: url.to_owned().into(),
        status: http::StatusCode::FORBIDDEN,
        detail: "fixture-echoed-authorization".to_owned().into(),
    };
    let wrapped = GuildConfigApiError::from(http);
    let secrets = [
        url,
        "fixture-user",
        "fixture-password",
        "fixture-webhook-token",
        "fixture-query-secret",
        "fixture-echoed-authorization",
    ];
    assert_redacted(&wrapped, &secrets);
    for secret in secrets {
        assert!(!wrapped.to_string().contains(secret));
    }
}

#[test]
fn status_detail_echoing_a_webhook_url_is_redacted_wholesale() {
    // A remote body can echo the request URL (webhook token plus query) or
    // an Authorization value back at us; the `detail` field must redact the
    // echo wholesale rather than truncate around it.
    let error = HttpError::Status {
        url: "https://discord.invalid/api/webhooks/1/unrelated"
            .to_owned()
            .into(),
        status: http::StatusCode::BAD_REQUEST,
        detail: "https://discord.invalid/api/webhooks/1/fixture-echoed-token?key=fixture-echoed-query with header fixture-echoed-auth"
            .to_owned()
            .into(),
    };
    let secrets = [
        "fixture-echoed-token",
        "fixture-echoed-query",
        "fixture-echoed-auth",
    ];
    assert_redacted(&error, &secrets);
    for secret in secrets {
        assert!(!error.to_string().contains(secret));
    }
}
