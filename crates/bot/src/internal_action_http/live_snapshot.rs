//! Private receiver for the Web hub's signed snapshot pull.

use super::{
    now_ms, wire_headers, AuthenticatedRequest, ClockGuard, ErrorCode, Failure, TokenBuckets,
    ACTIONS_PATH, MAX_BODY_BYTES, MAX_REQUESTS, SKEW_SECONDS,
};
use axum::{
    body::to_bytes,
    extract::Request,
    http::{header, Method, StatusCode},
    response::{IntoResponse, Response},
};
use std::{
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    sync::{RwLock, Semaphore},
    time::{timeout_at, Instant},
};
use two_bot::live_gateway::{LiveGateway, SequenceWatermark};
use two_bot_core::{
    internal_action_store::InternalActionStore,
    internal_actions::{KeyRing, LiveSnapshotReadAuthority, SigningKey},
    realtime_wire::{Id, WireTime},
};

// Leave room for the response in the Web client's 2.5-second pull budget.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

struct SnapshotScope {
    key_id: String,
    guild_id: Id,
    keys: KeyRing,
}

/// Accepts only a dedicated, guild-scoped snapshot credential. The caller
/// must supply the same ordered owner that processes the guild's dispatches.
pub struct LiveSnapshotEndpoint<W = sqlx::PgPool> {
    owner: Weak<RwLock<LiveGateway<W>>>,
    scope: Option<SnapshotScope>,
    store: InternalActionStore,
    capacity: Semaphore,
    clock: Mutex<ClockGuard>,
    buckets: Mutex<TokenBuckets>,
}

impl<W: SequenceWatermark> LiveSnapshotEndpoint<W> {
    /// Construct from trusted server state. With no credential scope, the
    /// endpoint refuses all reads without touching the store or owner.
    pub fn new(
        owner: Arc<RwLock<LiveGateway<W>>>,
        store: InternalActionStore,
        scope: Option<(SigningKey, Id)>,
    ) -> Self {
        Self {
            owner: Arc::downgrade(&owner),
            scope: scope.map(|(key, guild_id)| SnapshotScope {
                key_id: key.id.clone(),
                guild_id,
                keys: KeyRing::new(vec![key]),
            }),
            store,
            capacity: Semaphore::new(MAX_REQUESTS),
            clock: Mutex::new(ClockGuard::new()),
            buckets: Mutex::new(TokenBuckets::new()),
        }
    }

    pub async fn handle(&self, request: Request) -> Response {
        let id = crate::logging::incoming_request_id(request.headers());
        match self.receive(request).await {
            Ok(bytes) => (
                [
                    (header::CONTENT_TYPE, "application/json"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                bytes,
            )
                .into_response(),
            Err(failure) => failure.response(&id),
        }
    }

    async fn receive(&self, request: Request) -> Result<Vec<u8>, Failure> {
        if request.method() != Method::POST
            || request.uri().path_and_query().map(|path| path.as_str()) != Some(ACTIONS_PATH)
        {
            return Err(Failure::http(
                StatusCode::NOT_FOUND,
                "not_found",
                false,
                ErrorCode::Malformed,
            ));
        }
        let scope = self
            .scope
            .as_ref()
            .ok_or_else(|| Failure::code(ErrorCode::ActionNotAllowed))?;
        let _permit = self
            .capacity
            .try_acquire()
            .map_err(|_| Failure::code(ErrorCode::RateLimited))?;
        let deadline = Instant::now() + SNAPSHOT_TIMEOUT;
        let result = timeout_at(deadline, self.read(scope, request)).await;
        match result {
            Ok(result) if Instant::now() < deadline => result,
            _ => Err(Failure::code(ErrorCode::UpstreamTimeout)),
        }
    }

    async fn read(&self, scope: &SnapshotScope, request: Request) -> Result<Vec<u8>, Failure> {
        let (parts, body) = request.into_parts();
        let headers = wire_headers(&parts.headers)?;
        let raw = to_bytes(body, MAX_BODY_BYTES).await.map_err(|_| {
            Failure::http(
                StatusCode::PAYLOAD_TOO_LARGE,
                "malformed",
                false,
                ErrorCode::Malformed,
            )
        })?;
        let verified = AuthenticatedRequest::verify(
            &headers.auth,
            &raw,
            &scope.keys,
            SKEW_SECONDS,
            now_ms(),
            &mut self.clock.lock().expect("clock lock"),
        )
        .map_err(Failure::from_action)?;
        let burned = verified
            .burn_durably(&self.store)
            .await
            .map_err(Failure::from_action)?;
        let authority = LiveSnapshotReadAuthority {
            key_id: &scope.key_id,
            guild_id: scope.guild_id,
        };
        let grant = burned
            .authorize_live_snapshot(
                Some(&authority),
                &mut self.buckets.lock().expect("buckets lock"),
            )
            .map_err(Failure::from_action)?;

        let Some(owner) = self.owner.upgrade() else {
            return Err(Failure::code(ErrorCode::DiscordUnavailable));
        };
        let owner = owner.read().await;
        owner
            .read_snapshot(grant, || WireTime(time::OffsetDateTime::now_utc()))
            .ok_or_else(|| Failure::code(ErrorCode::DiscordUnavailable))
    }
}

#[cfg(test)]
mod tests;
