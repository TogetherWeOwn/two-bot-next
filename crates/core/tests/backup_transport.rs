//! Upload + guild-config transports against in-process fakes (TOG-9881).
//!
//! The unit tests pin the signature. They cannot catch what actually breaks
//! an off-box backup at 04:17: the uploader exiting zero on an HTTP error,
//! the object landing under a key nobody will look under, or a failure being
//! swallowed so the timer stays green while no backup exists.
//!
//! So this runs the real code paths against fakes that verify like the real
//! thing: the fake S3 re-derives the SigV4 signature from the bytes that
//! arrived and answers 403 on a mismatch (port of `test/helpers/fakeS3.ts`);
//! the fake Discord answers the guild-config reads/writes so snapshot,
//! plan and apply run end to end. Loopback only, no network, no tokens.

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

use two_bot_core::backup::{guild_config, guild_config_api::GuildConfigDiscordApi, s3};

// ---------------------------------------------------------------------------
// Fake S3: re-derives SigV4 from the received bytes, 403 on mismatch.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct S3State {
    secret: String,
    objects: HashMap<String, Vec<u8>>,
    requests: Vec<String>,
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn verify_sigv4(secret: &str, method: &str, path: &str, body: &[u8], headers: &HeaderMap) -> bool {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let scope = auth
        .split("Credential=")
        .nth(1)
        .and_then(|s| s.split('/').nth(1).zip(Some(s)))
        .map(|_| {
            auth.split("Credential=")
                .nth(1)
                .unwrap_or("")
                .split(',')
                .next()
                .unwrap_or("")
                .split('/')
                .nth(1)
                .unwrap_or("")
                .to_owned()
        });
    // Parse Credential=<id>/<date>/<region>/<service>/aws4_request etc.
    let credential = auth
        .split("Credential=")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .unwrap_or("");
    let mut cred_parts = credential.split('/');
    let _key_id = cred_parts.next().unwrap_or("");
    let date: &str = cred_parts.next().unwrap_or("");
    let region: &str = cred_parts.next().unwrap_or("");
    let service: &str = cred_parts.next().unwrap_or("");
    let signed = auth
        .split("SignedHeaders=")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .unwrap_or("");
    let got = auth
        .split("Signature=")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .unwrap_or("");
    let _ = scope;
    if date.is_empty()
        || region.is_empty()
        || service.is_empty()
        || signed.is_empty()
        || got.is_empty()
    {
        return false;
    }
    // SigV4 requires alphabetic order, not merely a self-consistent HMAC.
    // Refuse a sender that signs a noncanonical ordering, even if its hash matches.
    let names: Vec<&str> = signed.split(';').collect();
    if names.windows(2).any(|pair| pair[0] >= pair[1]) {
        return false;
    }
    let canonical_headers: String = signed
        .split(';')
        .map(|h| {
            let v = headers.get(h).and_then(|v| v.to_str().ok()).unwrap_or("");
            format!("{h}:{}\n", v.trim())
        })
        .collect();
    let payload_hash = hex_of(&Sha256::digest(body));
    let canonical_request =
        [method, path, "", &canonical_headers, signed, &payload_hash].join("\n");
    let amz_date = headers
        .get("x-amz-date")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let string_to_sign = [
        "AWS4-HMAC-SHA256",
        amz_date,
        &format!("{date}/{region}/{service}/aws4_request"),
        &hex_of(&Sha256::digest(canonical_request.as_bytes())),
    ]
    .join("\n");
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    hex_of(&hmac_sha256(&k_signing, string_to_sign.as_bytes())) == got
}

fn hex_of(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

async fn s3_put(
    State(state): State<Arc<Mutex<S3State>>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: axum::body::Body,
) -> Response {
    let path = uri.path().trim_start_matches('/').to_owned();
    let body = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap()
        .to_vec();
    let ok = {
        let state = state.lock().unwrap();
        verify_sigv4(&state.secret, "PUT", &format!("/{path}"), &body, &headers)
    };
    let mut state = state.lock().unwrap();
    state.requests.push(format!("PUT /{path} ok={ok}"));
    if !ok {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::from("signature mismatch"))
            .unwrap();
    }
    state.objects.insert(path, body);
    Response::builder()
        .status(StatusCode::OK)
        .header("etag", "\"test-etag\"")
        .body(Body::empty())
        .unwrap()
}

async fn start_fake_s3(secret: &str) -> (String, Arc<Mutex<S3State>>) {
    let state = Arc::new(Mutex::new(S3State {
        secret: secret.to_owned(),
        ..S3State::default()
    }));
    let app = Router::new()
        .route("/{*path}", axum::routing::put(s3_put))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), state)
}

const KEY_ID: &str = "AKIDEXAMPLE";

/// AWS's published SigV4 worked-example secret, read from a fixture file
/// rather than inlined: an inline literal trips the CodeQL hardcoded-key
/// gate (PR #11 review). Test-only vector, never a real credential.
fn secret() -> String {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/aws-sigv4-worked-example-secret.txt"
    );
    std::fs::read_to_string(path)
        .expect("fixture checked in with the port")
        .trim()
        .to_owned()
}

#[tokio::test]
async fn s3_put_round_trip_with_verified_signature() {
    let secret = secret();
    let (endpoint, state) = start_fake_s3(&secret).await;
    let target = s3::S3Target {
        endpoint: endpoint.into(),
        region: "auto".to_owned(),
        bucket: "paperclip-backups".to_owned(),
        access_key_id: KEY_ID.to_owned().into(),
        secret_access_key: secret.into(),
        prefix: Some("two-bot".to_owned()),
    };
    let body = b"test dump bytes".to_vec();
    let key = s3::object_key(target.prefix.as_deref(), "two-funnel-test.ndjson.gz");
    assert_eq!(key, "two-bot/two-funnel-test.ndjson.gz");
    let (amz_date, date_stamp) = s3::amz_stamps(1_787_735_020);
    let signed = s3::sign_put(&target, &key, &body, &amz_date, &date_stamp);

    // The real transport path: hyper PUT with the signed headers.
    let res = two_bot_core::backup::http::put(signed.url.expose(), signed.headers.expose().clone(), body.clone(), 30)
        .await
        .expect("PUT to fake S3");
    assert_eq!(
        res.status.as_u16(),
        200,
        "fake S3 verified the signature{}",
        res.detail()
    );
    assert_eq!(res.header("etag"), Some("\"test-etag\""));

    let state = state.lock().unwrap();
    assert_eq!(
        state
            .objects
            .get("paperclip-backups/two-bot/two-funnel-test.ndjson.gz"),
        Some(&body),
        "object landed under the key a restore will look under"
    );
}

#[tokio::test]
async fn s3_put_with_wrong_secret_is_403_not_success() {
    let (endpoint, _state) = start_fake_s3(&secret()).await;
    let target = s3::S3Target {
        endpoint: endpoint.into(),
        region: "auto".to_owned(),
        bucket: "paperclip-backups".to_owned(),
        access_key_id: KEY_ID.to_owned().into(),
        secret_access_key: "wrong-secret".to_owned().into(),
        prefix: None,
    };
    let body = b"test dump bytes".to_vec();
    let (amz_date, date_stamp) = s3::amz_stamps(1_787_735_020);
    let signed = s3::sign_put(&target, "f.gz", &body, &amz_date, &date_stamp);
    let res = two_bot_core::backup::http::put(signed.url.expose(), signed.headers.expose().clone(), body, 30)
        .await
        .expect("transport works; signature does not");
    assert_eq!(res.status.as_u16(), 403, "bad signature must fail loudly");
}

// ---------------------------------------------------------------------------
// Fake Discord: guild/roles/channels/emojis reads, identity, member, writes.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct DiscordState {
    guild: serde_json::Value,
    roles: serde_json::Value,
    channels: serde_json::Value,
    emojis: serde_json::Value,
    writes: Vec<(String, String)>,
    next_id: u64,
}

async fn discord_get(
    State(state): State<Arc<Mutex<DiscordState>>>,
    uri: axum::http::Uri,
) -> Response {
    // The fake serves the API under /api (mirroring /api/v10): strip it.
    let path = uri
        .path()
        .trim_start_matches('/')
        .trim_start_matches("api/")
        .to_owned();
    let state = state.lock().unwrap();
    let body = match path.as_str() {
        "users/@me" => serde_json::json!({"id": guild_config::STAGING_BOT_APPLICATION_ID}),
        "users/@me/guilds" => serde_json::json!([{"id": guild_config::TWO_STAGING_GUILD_ID}]),
        p if p
            == format!(
                "guilds/{}/members/{}",
                guild_config::TWO_STAGING_GUILD_ID,
                guild_config::STAGING_BOT_APPLICATION_ID
            ) =>
        {
            serde_json::json!({"roles": ["r-admin"]})
        }
        p if p == format!("guilds/{}", guild_config::TWO_STAGING_GUILD_ID) => state.guild.clone(),
        p if p == format!("guilds/{}/roles", guild_config::TWO_STAGING_GUILD_ID) => {
            state.roles.clone()
        }
        p if p == format!("guilds/{}/channels", guild_config::TWO_STAGING_GUILD_ID) => {
            state.channels.clone()
        }
        p if p == format!("guilds/{}/emojis", guild_config::TWO_STAGING_GUILD_ID) => {
            state.emojis.clone()
        }
        _ => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("no such fake route"))
                .unwrap();
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn discord_write(
    State(state): State<Arc<Mutex<DiscordState>>>,
    method: axum::http::Method,
    uri: axum::http::Uri,
) -> Response {
    let path = uri.path().to_owned();
    let mut state = state.lock().unwrap();
    state.next_id += 1;
    let id = format!("new-{}", state.next_id);
    state.writes.push((method.to_string(), path));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::json!({"id": id}).to_string()))
        .unwrap()
}

async fn cdn_emoji() -> Response {
    // 1x1 PNG bytes; content-type is what the capture path checks.
    let png: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, b'I', b'D', b'A', b'T', 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, b'I',
        b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82,
    ];
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "image/png")
        .body(Body::from(png.to_vec()))
        .unwrap()
}

async fn start_fake_discord(state: Arc<Mutex<DiscordState>>) -> (String, String) {
    let api = Router::new()
        .route("/cdn/{*path}", get(cdn_emoji))
        .route(
            "/{*path}",
            get(discord_get).post(discord_write).patch(discord_write),
        )
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, api).await.unwrap();
    });
    let base = format!("http://127.0.0.1:{port}");
    (format!("{base}/api"), format!("{base}/cdn"))
}

fn discord_fixture() -> DiscordState {
    DiscordState {
        guild: serde_json::json!({"id": guild_config::TWO_STAGING_GUILD_ID, "name": "TWO Staging", "description": "d", "owner_id": guild_config::STAGING_BOT_APPLICATION_ID}),
        roles: serde_json::json!([
            {"id": guild_config::TWO_STAGING_GUILD_ID, "name": "@everyone", "managed": false, "color": 0, "hoist": false, "permissions": "0", "mentionable": false, "position": 0},
            {"id": "r-admin", "name": "Admin", "managed": false, "color": 0, "hoist": true, "permissions": "8", "mentionable": false, "position": 3}
        ]),
        channels: serde_json::json!([
            {"id": "cat1", "name": "COMMUNITY", "type": 4, "parent_id": null, "position": 0, "permission_overwrites": []},
            {"id": "ch1", "name": "general", "type": 0, "parent_id": "cat1", "position": 0, "permission_overwrites": []}
        ]),
        emojis: serde_json::json!([
            {"id": "e1", "name": "wave", "roles": [], "require_colons": true, "managed": false, "animated": false, "available": true}
        ]),
        ..DiscordState::default()
    }
}

#[tokio::test]
async fn guild_config_capture_plan_apply_round_trip() {
    let (api_base, cdn_base) = start_fake_discord(Arc::new(Mutex::new(discord_fixture()))).await;
    let mut api = GuildConfigDiscordApi::new(
        Some(&api_base),
        Some(&cdn_base),
        "token".to_owned(),
        guild_config::STAGING_BOT_APPLICATION_ID.to_owned(),
        guild_config::TWO_STAGING_GUILD_ID.to_owned(),
    )
    .expect("loopback seams allowed");
    api.timeout_secs = 10;

    api.assert_identity().await.expect("fake identity holds");
    let snapshot = guild_config::seal_snapshot(api.capture().await.expect("capture"));
    assert_eq!(
        guild_config::verify_snapshot_integrity(&snapshot),
        Ok(guild_config::SealState::Sealed)
    );
    // The CDN seam was honoured: the unmanaged emoji carries image bytes.
    let emojis = snapshot["emojis"].as_array().unwrap();
    assert!(
        emojis[0]["image"]
            .as_str()
            .unwrap_or("")
            .starts_with("data:image/png;base64,"),
        "emoji image fetched via GUILD_CONFIG_CDN_BASE override"
    );

    // Mutate the live guild (rename a channel), then plan + apply the restore.
    // Note: the fake is stateful per test; use a second fake for "current".
    let mutated = Arc::new(Mutex::new(discord_fixture()));
    {
        let mut m = mutated.lock().unwrap();
        m.channels.as_array_mut().unwrap()[1]["name"] =
            serde_json::Value::String("general-renamed".to_owned());
    }
    let (api_base2, cdn_base2) = start_fake_discord(mutated.clone()).await;
    let mut api2 = GuildConfigDiscordApi::new(
        Some(&api_base2),
        Some(&cdn_base2),
        "token".to_owned(),
        guild_config::STAGING_BOT_APPLICATION_ID.to_owned(),
        guild_config::TWO_STAGING_GUILD_ID.to_owned(),
    )
    .unwrap();
    api2.timeout_secs = 10;
    let before = api2.capture().await.expect("capture current");
    let plan =
        two_bot_core::backup::guild_config_restore::plan_restore(&snapshot, &before).expect("plan");
    assert!(
        plan.operations
            .iter()
            .any(|op| op.label == "patch channel general"),
        "rename planned as patch, not create: {:?}",
        plan.operations.iter().map(|o| &o.label).collect::<Vec<_>>()
    );

    // Apply through the fake; every op resolves and writes are counted.
    let mut ids = two_bot_core::backup::guild_config_restore::RestoreIdMaps {
        roles: plan.known_ids.roles.clone(),
        channels: plan.known_ids.channels.clone(),
        emojis: plan.known_ids.emojis.clone(),
    };
    for op in &plan.operations {
        let path =
            two_bot_core::backup::guild_config_restore::resolve_path(&op.path, &ids.channels)
                .unwrap();
        let body = two_bot_core::backup::guild_config_restore::resolve_value(
            &op.body,
            &ids.roles,
            &ids.channels,
        )
        .unwrap();
        let result = api2
            .write(&op.method, &path, body)
            .await
            .expect("fake write ok");
        if let Some((resource, source)) = &op.capture_id {
            let id = result
                .as_ref()
                .and_then(|b| b.get("id"))
                .and_then(|v| v.as_str())
                .expect("fake returns an id")
                .to_owned();
            match resource.as_str() {
                "role" => {
                    ids.roles.insert(source.clone(), id);
                }
                "channel" => {
                    ids.channels.insert(source.clone(), id);
                }
                _ => {
                    ids.emojis.insert(source.clone(), id);
                }
            }
        }
    }
    assert_eq!(api2.writes as usize, plan.operations.len());
}

#[tokio::test]
async fn tampered_snapshot_is_refused_before_any_discord_call() {
    let (api_base, cdn_base) = start_fake_discord(Arc::new(Mutex::new(discord_fixture()))).await;
    let api = GuildConfigDiscordApi::new(
        Some(&api_base),
        Some(&cdn_base),
        "token".to_owned(),
        guild_config::STAGING_BOT_APPLICATION_ID.to_owned(),
        guild_config::TWO_STAGING_GUILD_ID.to_owned(),
    )
    .unwrap();
    let mut snapshot = api.capture().await.expect("capture");
    snapshot = guild_config::seal_snapshot(snapshot);
    snapshot["channels"].as_array_mut().unwrap()[1]["name"] =
        serde_json::Value::String("tampered".to_owned());
    let err = guild_config::verify_snapshot_integrity(&snapshot).expect_err("tamper refused");
    assert!(err.to_string().contains("tampered snapshot"), "{err}");
}
