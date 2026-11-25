//! Read-only scheduled-event permission evidence from the Bot's gateway.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{header, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{RwLock, Semaphore};
use twilight_model::id::{
    marker::{GuildMarker, UserMarker},
    Id,
};
use two_bot_core::{
    internal_actions::{
        parse_keys, valid_nonce_format, within_skew, AuthHeaders, KeyRing, NonceCache,
        NONCE_TTL_SECONDS, SKEW_SECONDS,
    },
    ClockGuard,
};
use two_bot_discord::{EventPermissionEvidence, SharedCache};

use crate::{gateway::GatewayState, internal_action_http::signed_json_headers};

const PATH: &str = "/ops/event-permissions";
const ENABLED: &str = "TWO_EVENT_PERMISSION_READBACK";
const BOT_ID: &str = "TWO_EVENT_PERMISSION_READBACK_BOT_ID";
const KEY: &str = "TWO_EVENT_PERMISSION_READBACK_KEY";
const MAX_NONCES: usize = 256;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    guild_id: String,
    bot_id: String,
}

pub(crate) struct Readback {
    guild: Id<GuildMarker>,
    bot: Id<UserMarker>,
    keys: KeyRing,
    cache: Arc<SharedCache>,
    replay: Mutex<(ClockGuard, NonceCache)>,
    capacity: Semaphore,
}

impl Readback {
    pub(crate) fn from_env(guild: Option<u64>, cache: Arc<SharedCache>) -> Option<Self> {
        let configured = Self::from_lookup(guild, cache, |name| std::env::var(name).ok());
        if configured.is_none() && std::env::var(ENABLED).as_deref() == Ok("1") {
            tracing::warn!("event permission readback configuration invalid");
        }
        configured
    }

    fn from_lookup(
        guild: Option<u64>,
        cache: Arc<SharedCache>,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Option<Self> {
        if lookup(ENABLED).as_deref() != Some("1") {
            return None;
        }
        let guild = Id::new_checked(guild?)?;
        let bot = parse_id(&lookup(BOT_ID)?)?;
        let keys = parse_keys(&lookup(KEY)?).ok()?;
        let [key] = keys.as_slice() else { return None };
        let existing = lookup("TWO_INTERNAL_KEYS").unwrap_or_default();
        if key.id.len() > 128
            || existing.split(',').any(|entry| {
                entry
                    .split_once(':')
                    .is_some_and(|(_, secret)| secret.trim().as_bytes() == key.secret.expose())
            })
            || lookup("TWO_INTERNAL_LIVE_SNAPSHOT_SECRET")
                .is_some_and(|secret| secret.as_bytes() == key.secret.expose())
        {
            return None;
        }
        Some(Self {
            guild,
            bot,
            keys: KeyRing::new(keys),
            cache,
            replay: Mutex::new((ClockGuard::new(), NonceCache::new(NONCE_TTL_SECONDS))),
            capacity: Semaphore::new(1),
        })
    }

    fn authenticate(&self, headers: &AuthHeaders<'_>, body: &[u8]) -> Option<()> {
        if !valid_nonce_format(headers.nonce)
            || !self.keys.verify_post(
                PATH,
                headers.key_id,
                headers.signature,
                headers.timestamp,
                headers.nonce,
                body,
            )
        {
            return None;
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
        let mut replay = self.replay.lock().ok()?;
        let (clock, nonces) = &mut *replay;
        let guarded = clock.evaluate(now.as_millis().try_into().ok()?).ok()?;
        if !within_skew(headers.timestamp, SKEW_SECONDS, guarded / 1000) {
            return None;
        }
        nonces.sweep(guarded);
        (nonces.len() < MAX_NONCES && nonces.offer(&headers.nonce.to_ascii_lowercase(), guarded))
            .then_some(())
    }

    fn read(&self, gateway: &RwLock<GatewayState>) -> Option<EventPermissionEvidence> {
        // Keep readiness and all policy inputs fenced through the projection.
        let state = gateway.try_read().ok()?;
        if *state != GatewayState::Connected {
            return None;
        }
        self.cache.event_permission_evidence(self.guild, self.bot)
    }
}

fn parse_id<T>(value: &str) -> Option<Id<T>> {
    let id = Id::new_checked(value.parse().ok()?)?;
    (id.to_string() == value).then_some(id)
}

pub(crate) fn router(
    readback: Option<Arc<Readback>>,
    gateway: Arc<RwLock<GatewayState>>,
) -> Router {
    let Some(readback) = readback else {
        return Router::new();
    };
    Router::new()
        .route(PATH, any(handle))
        .with_state((readback, gateway))
}

async fn handle(
    State((readback, gateway)): State<(Arc<Readback>, Arc<RwLock<GatewayState>>)>,
    request: Request,
) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        serve_read(&readback, &gateway, request).await.map(Json),
    )
        .into_response()
}

async fn serve_read(
    readback: &Readback,
    gateway: &RwLock<GatewayState>,
    request: Request,
) -> Result<Value, StatusCode> {
    if request.method() != Method::POST || request.uri().query().is_some() {
        return Err(StatusCode::NOT_FOUND);
    }
    let (parts, body) = request.into_parts();
    let headers = signed_json_headers(&parts.headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let raw = tokio::time::timeout(Duration::from_secs(2), to_bytes(body, 256))
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let _permit = readback
        .capacity
        .try_acquire()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;
    readback
        .authenticate(&headers, &raw)
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let target: Target = serde_json::from_slice(&raw).map_err(|_| StatusCode::BAD_REQUEST)?;
    if parse_id::<GuildMarker>(&target.guild_id) != Some(readback.guild)
        || parse_id::<UserMarker>(&target.bot_id) != Some(readback.bot)
    {
        return Err(StatusCode::NOT_FOUND);
    }
    let permissions = readback
        .read(gateway)
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(json!({
        "v": 1, "scope_nonce": headers.nonce, "permissions": permissions,
        "observed_at_unix_seconds": time::OffsetDateTime::now_utc().unix_timestamp(),
    }))
}

#[cfg(test)]
mod tests;
