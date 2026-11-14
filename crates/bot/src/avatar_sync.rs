//! One-shot staging bot avatar sync to the owner's Icon 1.
//!
//! The staging bot sets its own avatar through the Discord API
//! (`PATCH /users/@me`) using the token it already holds at runtime, so no
//! portal login and no token copy is needed. It runs only when both the
//! deployment-environment marker the Worker carries in its staging vars and
//! the exact one-shot switch from the generated staging deploy config agree
//! ([`avatar_sync_active`]); production carries neither, so it refuses there
//! by construction. The image is the committed Icon 1 PNG bundled into the
//! binary, never read from disk at runtime. Only the HTTP status and the
//! before/after avatar hashes reach the log; the token and the image bytes
//! never do. A failure never blocks boot: the gateway still starts.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use std::future::IntoFuture as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const SYNC_TIMEOUT: Duration = Duration::from_secs(15);

/// Generated-config switch that arms the one-shot avatar sync. Stamped only
/// into the generated staging deploy config by `scripts/staging_rollout.py`
/// (`apply_avatar_sync_flag`, dispatch-only `--set-bot-avatar-once`) for a
/// manual dispatch; the committed `wrangler.toml` must never carry it
/// (enforced by `wrangler/scripts/check-env-bindings.py`). The Worker
/// forwards it only after durably consuming the per-icon authorization in
/// `avatarStartOptions` while `TWO_DEPLOY_ENV` is exactly `"staging"`
/// (`wrangler/src/index.ts`), never through class defaults or the flag allowlist.
pub const AVATAR_SYNC_VAR: &str = "TWO_SET_BOT_AVATAR_ONCE";

/// Deployment-environment marker: `wrangler.toml [env.staging.vars]` carries
/// exactly `"staging"` (committed, staging-only), and it reaches the Container
/// through the generic flag allowlist (`FORWARDED_FLAGS` in
/// `wrangler/src/container-env.ts`). Production and top level never carry it.
pub const DEPLOY_ENV_VAR: &str = "TWO_DEPLOY_ENV";

/// Expected Icon 1 avatar hash once staging reports it.
///
/// Pinned after the first armed staging run logs `bot avatar sync applied`
/// with the new `avatar` hash: paste that hash here so later armed boots skip
/// the PATCH when the current hash already matches. `None` until the first
/// staging evidence lands, so the first armed boot always PATCHes once to
/// learn it. This is an additional no-write guard, not one-shot authorization:
/// the Worker's durable consume-before-start receipt prevents replay across
/// restarts, including failed or uncertain writes where a hash cannot help.
const EXPECTED_ICON_HASH: Option<&str> = None;

/// True when the current avatar does not match the pinned Icon 1 hash.
/// Unknown (`None`) expected means sync, so the first armed run learns it.
#[must_use]
fn avatar_needs_sync(current: Option<&str>) -> bool {
    match (current, EXPECTED_ICON_HASH) {
        (Some(current), Some(expected)) => current != expected,
        _ => true,
    }
}

/// Committed Icon 1 dark-background PNG (512x512), the staging bot avatar.
const ICON_PNG: &[u8] = include_bytes!("../../../assets/brand/two-icon-1-dark-512.png");

/// PNG file signature: every valid PNG starts with these eight bytes.
const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// Largest avatar payload accepted. The bundled Icon 1 PNG is ~29 KiB, so
/// this leaves room for a future icon swap while refusing absurd uploads.
const MAX_ICON_BYTES: usize = 256 * 1024;

/// Data-URI prefix Discord requires for an uploaded PNG avatar.
const DATA_URI_PREFIX: &str = "data:image/png;base64,";

/// Pure fence: the sync runs only when the staging marker is exactly
/// `staging` AND the one-shot switch is exactly `1`. Every other
/// combination (unset, `production`, a typo, a truthy spelling other than
/// `1`) is off, so production refuses it by construction.
#[must_use]
pub fn avatar_sync_active(deploy_env: Option<&str>, switch: Option<&str>) -> bool {
    deploy_env == Some("staging") && switch == Some("1")
}

/// The fence read once from the process environment at boot.
#[must_use]
pub fn avatar_sync_from_env() -> bool {
    let deploy_env = std::env::var(DEPLOY_ENV_VAR).ok();
    let switch = std::env::var(AVATAR_SYNC_VAR).ok();
    avatar_sync_active(deploy_env.as_deref(), switch.as_deref())
}

/// Fixed-vocabulary outcome: the PATCH status plus the avatar hashes before
/// and after. Hashes are public CDN path segments, safe to log; the token
/// and the image bytes never enter a log line.
#[derive(Debug, PartialEq, Eq)]
pub struct AvatarSyncOutcome {
    pub http_status: u16,
    pub previous_avatar: Option<String>,
    pub avatar: Option<String>,
}

/// Build the Discord image-data URI for raw PNG bytes: magic check, size
/// cap, then base64. Errors are fixed codes that never echo the bytes.
fn icon_data_uri_from_bytes(bytes: &[u8]) -> Result<String, &'static str> {
    if bytes.len() > MAX_ICON_BYTES {
        return Err("avatar_image_too_large");
    }
    if bytes.len() < PNG_MAGIC.len() || bytes[..PNG_MAGIC.len()] != PNG_MAGIC {
        return Err("avatar_image_not_png");
    }
    let mut uri = String::with_capacity(DATA_URI_PREFIX.len() + bytes.len() * 4 / 3 + 4);
    uri.push_str(DATA_URI_PREFIX);
    uri.push_str(&STANDARD.encode(bytes));
    Ok(uri)
}

/// The Discord image-data URI for the bundled Icon 1 PNG.
fn icon_data_uri() -> Result<String, &'static str> {
    icon_data_uri_from_bytes(ICON_PNG)
}

/// Run the one-shot sync with an already-built client: read the current user
/// for the before-hash (returning early with no PATCH when it already matches
/// the pinned Icon 1 hash), then validate the bundled image and PATCH the
/// avatar, returning the status plus both hashes. Every error is a fixed
/// code: HTTP bodies and transport errors can carry configuration, so they
/// are never formatted into the result.
pub async fn run_once(client: &twilight_http::Client) -> Result<AvatarSyncOutcome, &'static str> {
    run_with_deadline(client, SYNC_TIMEOUT).await
}

async fn run_with_deadline(
    client: &twilight_http::Client,
    deadline: Duration,
) -> Result<AvatarSyncOutcome, &'static str> {
    tokio::time::timeout(deadline, sync_avatar(client))
        .await
        .map_err(|_| "avatar_sync_timed_out")?
}

async fn sync_avatar(client: &twilight_http::Client) -> Result<AvatarSyncOutcome, &'static str> {
    let previous = client
        .current_user()
        .await
        .map_err(|_| "avatar_sync_lookup_failed")?
        .model()
        .await
        .map_err(|_| "avatar_sync_lookup_failed")?;
    let previous_avatar = previous.avatar.map(|hash| hash.to_string());
    // Additional no-write guard once the observed Icon 1 hash is pinned.
    // Durable Worker admission, not this comparison, enforces one-shot use.
    if !avatar_needs_sync(previous_avatar.as_deref()) {
        return Ok(AvatarSyncOutcome {
            http_status: 200,
            previous_avatar: previous_avatar.clone(),
            avatar: previous_avatar,
        });
    }
    let uri = icon_data_uri()?;
    let mut request = client
        .update_current_user()
        .avatar(Some(&uri))
        .into_future();
    // Twilight automatically retries 429s. Keep its rate-limit admission, but
    // cancel any second flight: a one-shot avatar write must never be replayed.
    let sent = AtomicBool::new(false);
    if !request.set_pre_flight(move || !sent.swap(true, Ordering::SeqCst)) {
        return Err("avatar_sync_retry_guard_missing");
    }
    let response = request.await.map_err(|_| "avatar_sync_request_failed")?;
    let http_status = response.status().get();
    let updated = response
        .model()
        .await
        .map_err(|_| "avatar_sync_request_failed")?;
    let avatar = updated.avatar.map(|hash| hash.to_string());
    if avatar.is_none() {
        return Err("avatar_sync_unconfirmed");
    }
    Ok(AvatarSyncOutcome {
        http_status,
        previous_avatar,
        avatar,
    })
}

/// Boot entrypoint: no-op unless the generated staging config armed the
/// switch. Uses the in-memory token, logs only status and hashes, and always
/// returns normally so a sync failure can never block the gateway. Runs at
/// most once per process: a second call in the same boot is a no-op. The
/// Worker consumes durable authorization before starting this process and
/// does not forward the switch to a replacement, regardless of this outcome.
pub async fn maybe_sync_on_boot(token: Option<&str>) {
    static SYNCED_ONCE: AtomicBool = AtomicBool::new(false);
    if SYNCED_ONCE.swap(true, Ordering::SeqCst) {
        return;
    }
    if !avatar_sync_from_env() {
        SYNCED_ONCE.store(false, Ordering::SeqCst);
        return;
    }
    let Some(token) = token.filter(|token| !token.is_empty()) else {
        tracing::error!(
            error_class = "avatar_sync_token_missing",
            "bot avatar sync armed but no token is configured"
        );
        return;
    };
    let client = twilight_http::Client::new(token.to_owned());
    match run_once(&client).await {
        Ok(outcome) => tracing::info!(
            http_status = outcome.http_status,
            previous_avatar = outcome.previous_avatar.as_deref().unwrap_or("none"),
            avatar = outcome.avatar.as_deref().unwrap_or("none"),
            "bot avatar sync applied",
        ),
        Err(reason) => tracing::error!(
            error_class = "avatar_sync_failed",
            reason,
            "bot avatar sync failed; continuing boot",
        ),
    }
}

#[cfg(test)]
mod avatar_sync_tests {
    use super::*;

    #[test]
    fn fence_needs_the_staging_marker_and_the_exact_switch() {
        assert!(avatar_sync_active(Some("staging"), Some("1")));
        for (deploy_env, switch) in [
            (None, None),
            (None, Some("1")),
            (Some("staging"), None),
            (Some("staging"), Some("")),
            (Some("staging"), Some("true")),
            (Some("staging"), Some("True")),
            (Some("staging"), Some(" staging")),
            (Some("production"), Some("1")),
            (Some("prod"), Some("1")),
            (Some(""), Some("1")),
        ] {
            assert!(
                !avatar_sync_active(deploy_env, switch),
                "deploy_env={deploy_env:?} switch={switch:?} must stay off"
            );
        }
    }

    #[test]
    fn unknown_expected_hash_always_syncs_first_run() {
        // Until the first armed staging run reports the Icon 1 hash and it is
        // pinned in EXPECTED_ICON_HASH, every current value (even none) syncs,
        // so the first armed boot learns the hash exactly once.
        assert!(avatar_needs_sync(None));
        assert!(avatar_needs_sync(Some("0123456789abcdef0123456789abcdef")));
        assert!(avatar_needs_sync(Some("")));
    }

    #[test]
    fn bundled_icon_builds_a_png_data_uri() {
        let uri = icon_data_uri().expect("bundled Icon 1 PNG must encode");
        assert!(uri.starts_with(DATA_URI_PREFIX), "must be a PNG data URI");
        let raw = STANDARD
            .decode(uri.trim_start_matches(DATA_URI_PREFIX))
            .expect("data URI body must be base64");
        assert_eq!(&raw[..PNG_MAGIC.len()], &PNG_MAGIC, "round trip keeps PNG");
        assert_eq!(raw, ICON_PNG, "round trip keeps every byte");
    }

    #[test]
    fn non_png_bytes_refuse_before_any_size_math() {
        assert_eq!(
            icon_data_uri_from_bytes(b"not a png"),
            Err("avatar_image_not_png")
        );
        assert_eq!(icon_data_uri_from_bytes(&[]), Err("avatar_image_not_png"));
        assert_eq!(
            icon_data_uri_from_bytes(&PNG_MAGIC[..7]),
            Err("avatar_image_not_png")
        );
    }

    #[test]
    fn oversize_png_refuses_even_with_valid_magic() {
        let mut bytes = vec![0u8; MAX_ICON_BYTES + 1];
        bytes[..PNG_MAGIC.len()].copy_from_slice(&PNG_MAGIC);
        assert_eq!(
            icon_data_uri_from_bytes(&bytes),
            Err("avatar_image_too_large")
        );
    }

    /// Minimal Discord user JSON the loopback fixture serves: only the
    /// fields twilight requires plus the avatar hash under test.
    /// `mfa_enabled` is required because the GET lookup parses as
    /// `CurrentUser`, whose field is a non-optional bool just like the real
    /// `/users/@me` response always carries.
    fn user_json(avatar: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "123456789012345678",
            "username": "staging-bot",
            "discriminator": "0",
            "bot": true,
            "mfa_enabled": true,
            "avatar": avatar,
        })
    }

    /// Loopback Discord double: records the PATCH body and auth shape, then
    /// serves canned GET/PATCH user payloads. Needs a Tokio context because
    /// the twilight client spawns its ratelimit actor.
    struct Loopback {
        addr: std::net::SocketAddr,
        seen_auth: std::sync::Arc<tokio::sync::Mutex<Vec<String>>>,
        seen_bodies: std::sync::Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    impl Loopback {
        async fn serve(
            get_avatar: &'static str,
            patch_avatar: &'static str,
            patch_status: u16,
        ) -> Self {
            use std::sync::Arc;
            use tokio::sync::Mutex;

            let seen_auth = Arc::new(Mutex::new(Vec::new()));
            let seen_bodies = Arc::new(Mutex::new(Vec::new()));
            let get_auth = Arc::clone(&seen_auth);
            let patch_auth = Arc::clone(&seen_auth);
            let bodies = Arc::clone(&seen_bodies);
            let app = axum::Router::new().route(
                "/api/v10/users/@me",
                axum::routing::get(move |headers: axum::http::HeaderMap| async move {
                    if let Some(value) = headers.get(axum::http::header::AUTHORIZATION) {
                        get_auth
                            .lock()
                            .await
                            .push(value.to_str().unwrap_or("unreadable").to_owned());
                    }
                    axum::Json(user_json(get_avatar))
                })
                .patch(
                    move |headers: axum::http::HeaderMap, body: String| async move {
                        if let Some(value) = headers.get(axum::http::header::AUTHORIZATION) {
                            patch_auth
                                .lock()
                                .await
                                .push(value.to_str().unwrap_or("unreadable").to_owned());
                        }
                        bodies.lock().await.push(body);
                        (
                            axum::http::StatusCode::from_u16(patch_status)
                                .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
                            axum::Json(user_json(patch_avatar)),
                        )
                    },
                ),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("loopback bind must succeed");
            let addr = listener.local_addr().expect("loopback addr must read");
            tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("loopback must serve");
            });
            Self {
                addr,
                seen_auth,
                seen_bodies,
            }
        }

        fn client(&self) -> twilight_http::Client {
            twilight_http::Client::builder()
                .proxy(self.addr.to_string(), true)
                .token("loopback-test-token".to_owned())
                .build()
        }
    }

    #[tokio::test]
    async fn sync_patches_the_avatar_and_reports_both_hashes() {
        let previous = "0123456789abcdef0123456789abcdef";
        let next = "fedcba9876543210fedcba9876543210";
        let loopback = Loopback::serve(previous, next, 200).await;
        let outcome = run_once(&loopback.client())
            .await
            .expect("loopback sync must succeed");

        assert_eq!(outcome.http_status, 200);
        assert_eq!(outcome.previous_avatar.as_deref(), Some(previous));
        assert_eq!(outcome.avatar.as_deref(), Some(next));

        let auth = loopback.seen_auth.lock().await;
        assert_eq!(auth.len(), 2, "GET and PATCH both authenticate");
        for value in auth.iter() {
            assert_eq!(value, "Bot loopback-test-token", "bot token auth scheme");
        }
        let bodies = loopback.seen_bodies.lock().await;
        assert_eq!(bodies.len(), 1, "exactly one avatar PATCH");
        let payload: serde_json::Value =
            serde_json::from_str(&bodies[0]).expect("PATCH body must be JSON");
        let avatar = payload
            .get("avatar")
            .and_then(serde_json::Value::as_str)
            .expect("PATCH body must carry the avatar data URI");
        assert!(avatar.starts_with(DATA_URI_PREFIX));
        assert!(avatar.len() > DATA_URI_PREFIX.len() + 1000);
    }

    #[tokio::test]
    async fn sync_without_a_confirmed_hash_fails_closed() {
        let previous = "0123456789abcdef0123456789abcdef";
        let app = axum::Router::new().route(
            "/api/v10/users/@me",
            axum::routing::get(move || async move { axum::Json(user_json(previous)) }).patch(
                || async {
                    axum::Json(serde_json::json!({
                        "id": "123456789012345678",
                        "username": "staging-bot",
                        "discriminator": "0",
                        "bot": true,
                        "avatar": null,
                    }))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind must succeed");
        let addr = listener.local_addr().expect("loopback addr must read");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("loopback must serve");
        });
        let client = twilight_http::Client::builder()
            .proxy(addr.to_string(), true)
            .token("loopback-test-token".to_owned())
            .build();
        assert_eq!(
            run_once(&client).await,
            Err("avatar_sync_unconfirmed"),
            "a PATCH that returns no hash must not read as applied"
        );
    }

    #[tokio::test]
    async fn failed_patch_surfaces_a_fixed_code() {
        let loopback = Loopback::serve(
            "0123456789abcdef0123456789abcdef",
            "fedcba9876543210fedcba9876543210",
            429,
        )
        .await;
        assert_eq!(
            run_once(&loopback.client()).await,
            Err("avatar_sync_request_failed"),
            "rate limiting must surface without echoing the response"
        );
        assert_eq!(loopback.seen_bodies.lock().await.len(), 1, "no 429 replay");
    }

    #[tokio::test]
    async fn uncertain_patch_times_out_without_replay() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let patches = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&patches);
        let app = axum::Router::new().route(
            "/api/v10/users/@me",
            axum::routing::get(|| async {
                axum::Json(user_json("0123456789abcdef0123456789abcdef"))
            })
            .patch(move || async move {
                seen.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
                axum::Json(user_json("fedcba9876543210fedcba9876543210"))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind must succeed");
        let addr = listener.local_addr().expect("loopback addr must read");
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let client = twilight_http::Client::builder()
            .proxy(addr.to_string(), true)
            .token("loopback-test-token".to_owned())
            .build();
        assert_eq!(
            run_with_deadline(&client, Duration::from_secs(1)).await,
            Err("avatar_sync_timed_out"),
        );
        assert_eq!(
            patches.load(Ordering::SeqCst),
            1,
            "no uncertain PATCH replay"
        );
        server.abort();
    }

    #[tokio::test]
    async fn stalled_lookup_has_a_total_deadline() {
        let app = axum::Router::new().route(
            "/api/v10/users/@me",
            axum::routing::get(|| async {
                std::future::pending::<()>().await;
                axum::Json(user_json("0123456789abcdef0123456789abcdef"))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback bind must succeed");
        let addr = listener.local_addr().expect("loopback addr must read");
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let client = twilight_http::Client::builder()
            .proxy(addr.to_string(), true)
            .token("loopback-test-token".to_owned())
            .build();
        assert_eq!(
            run_with_deadline(&client, Duration::from_millis(50)).await,
            Err("avatar_sync_timed_out"),
        );
        server.abort();
    }
}
