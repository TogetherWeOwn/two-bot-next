//! V1 Discord writes and permission evaluation. The lifecycle worker owns order,
//! persistence-before-move, compensation and retry scheduling; this adapter never
//! blindly retries a create whose response may have been lost.
//!
//! Sources:
//! <https://docs.rs/twilight-http/0.17.1/twilight_http/request/guild/struct.CreateGuildChannel.html>
//! <https://docs.discord.com/developers/topics/permissions#permission-overwrites>

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use http::{
    header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER},
    HeaderValue, Request,
};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client as HyperClient},
    rt::TokioExecutor,
};
use serde::Deserialize;
use tokio::time::Instant;
use twilight_http::{error::ErrorType, request::TryIntoRequest, Client};
use twilight_model::{
    channel::message::{component::Component, AllowedMentions},
    channel::{
        permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
        Channel, ChannelType, VideoQualityMode,
    },
    guild::{Permissions, Role},
    http::{
        attachment::Attachment,
        interaction::InteractionResponse,
        permission_overwrite::{
            PermissionOverwrite as HttpPermissionOverwrite,
            PermissionOverwriteType as HttpPermissionOverwriteType,
        },
    },
    id::{
        marker::{ApplicationMarker, InteractionMarker, RoleMarker},
        Id,
    },
};
use two_bot_core::{
    voice_rooms::{parse_retry_after_ms, CreatorChannel, MAX_USER_LIMIT},
    voice_text_channel::{ChannelOverwrite, OverwriteTarget, TextChannelPlan},
    Snowflake,
};

/// None means the member/role snapshot is incomplete: callers must fail closed.
/// Category overwrites are not applied a second time: a synced channel already
/// carries the category's overwrites, while an unsynced one has its own rules.
#[must_use]
pub fn effective_permissions(
    guild_id: Snowflake,
    owner_id: Snowflake,
    member_id: Snowflake,
    member_roles: &[Id<RoleMarker>],
    roles: &[Role],
    overwrites: &[PermissionOverwrite],
) -> Option<Permissions> {
    if member_id == owner_id {
        return Some(Permissions::all());
    }
    let everyone = roles.iter().find(|role| role.id.get() == guild_id)?;
    let mut permissions = everyone.permissions;
    for role_id in member_roles {
        permissions |= roles.iter().find(|role| role.id == *role_id)?.permissions;
    }
    if permissions.contains(Permissions::ADMINISTRATOR) {
        return Some(Permissions::all());
    }
    if let Some(overwrite) = overwrites.iter().find(|overwrite| {
        overwrite.kind == PermissionOverwriteType::Role && overwrite.id.get() == guild_id
    }) {
        permissions = (permissions & !overwrite.deny) | overwrite.allow;
    }
    let mut role_deny = Permissions::empty();
    let mut role_allow = Permissions::empty();
    for overwrite in overwrites.iter().filter(|overwrite| {
        overwrite.kind == PermissionOverwriteType::Role
            && member_roles.iter().any(|id| id.get() == overwrite.id.get())
            && overwrite.id.get() != guild_id
    }) {
        role_deny |= overwrite.deny;
        role_allow |= overwrite.allow;
    }
    permissions = (permissions & !role_deny) | role_allow;
    if let Some(overwrite) = overwrites.iter().find(|overwrite| {
        overwrite.kind == PermissionOverwriteType::Member && overwrite.id.get() == member_id
    }) {
        permissions = (permissions & !overwrite.deny) | overwrite.allow;
    }
    Some(permissions)
}

#[must_use]
pub fn can_manage_room(permissions: Option<Permissions>) -> bool {
    permissions.is_some_and(|permissions| {
        permissions.contains(
            Permissions::VIEW_CHANNEL
                | Permissions::CONNECT
                | Permissions::MANAGE_CHANNELS
                | Permissions::MOVE_MEMBERS,
        )
    })
}

/// V4 vote-kick enforcement: the member-scoped Connect deny is a permission
/// overwrite write (Manage Roles) and the disconnect is a Move Members write.
#[must_use]
pub fn can_enforce_kick(permissions: Option<Permissions>) -> bool {
    permissions.is_some_and(|permissions| {
        permissions.contains(
            Permissions::VIEW_CHANNEL | Permissions::MANAGE_ROLES | Permissions::MOVE_MEMBERS,
        )
    })
}

/// Attributes captured from the live creator, not from an earlier gateway frame.
/// Overwrites are resolved by the worker according to PermissionSource, then
/// included in the POST. No post-create permission patch is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomChannelAttributes {
    pub parent_id: Option<Snowflake>,
    pub bitrate: Option<u32>,
    pub rtc_region: Option<String>,
    pub video_quality_mode: Option<VideoQualityMode>,
    pub nsfw: bool,
    pub user_limit: u16,
    /// Create-time sorting position (V8 placement); `None` lets Discord append.
    pub position: Option<u64>,
    /// Bulk reorder of the category's channels the caller applies first,
    /// opening a free `position` (empty when it is free already). Not sent
    /// by `create_room`.
    pub respace: Vec<(Snowflake, u64)>,
    /// Position the caller uses instead of `position` when that reorder is
    /// refused (`None`: create without a position).
    pub fallback_position: Option<u64>,
    /// Empty means "include no overrides": the room syncs to its category.
    pub overwrites: Vec<PermissionOverwrite>,
}

impl RoomChannelAttributes {
    pub fn from_creator(
        settings: &CreatorChannel,
        channel: &Channel,
        overwrites: Vec<PermissionOverwrite>,
    ) -> Result<Self, RoomHttpError> {
        settings
            .validate()
            .map_err(|_| RoomHttpError::InvalidRequest)?;
        if channel.id.get() != settings.channel_id
            || channel.guild_id.map(Id::get) != Some(settings.guild_id)
            || channel.kind != ChannelType::GuildVoice
        {
            return Err(RoomHttpError::InvalidRequest);
        }
        let user_limit = settings
            .default_limit
            .unwrap_or(i64::from(channel.user_limit.unwrap_or(0)));
        if !(0..=MAX_USER_LIMIT).contains(&user_limit) {
            return Err(RoomHttpError::InvalidRequest);
        }
        let user_limit = user_limit as u16;
        Ok(Self {
            parent_id: channel.parent_id.map(Id::get),
            bitrate: channel.bitrate,
            rtc_region: channel.rtc_region.clone(),
            video_quality_mode: channel.video_quality_mode,
            nsfw: channel.nsfw.unwrap_or(false),
            user_limit,
            position: None,
            respace: Vec::new(),
            fallback_position: None,
            overwrites,
        })
    }
}

/// Map one pure V9 [`ChannelOverwrite`] to its Discord wire form. View is
/// the only bit ever set: allow grants View, deny denies it.
fn wire_overwrite(
    guild_id: Snowflake,
    overwrite: &ChannelOverwrite,
) -> Option<PermissionOverwrite> {
    let (kind, id) = match overwrite.target {
        OverwriteTarget::Everyone => (PermissionOverwriteType::Role, guild_id),
        OverwriteTarget::Role(role_id) => (PermissionOverwriteType::Role, role_id),
        OverwriteTarget::Member(member_id) => (PermissionOverwriteType::Member, member_id),
    };
    if id == 0 {
        return None;
    }
    let mut allow = Permissions::empty();
    let mut deny = Permissions::empty();
    if overwrite.allow_view {
        allow |= Permissions::VIEW_CHANNEL;
    }
    if overwrite.deny_view {
        deny |= Permissions::VIEW_CHANNEL;
    }
    Some(PermissionOverwrite {
        allow,
        deny,
        id: Id::new(id),
        kind,
    })
}

/// Full overwrite set for a V9 companion POST, with the bot's own View
/// allow appended (V9 AC5): a non-admin bot denied via @everyone would
/// otherwise get Missing Access on later overwrite edits and deletion.
/// Overwrites ride the create POST and are never patched afterwards (V8
/// rule). Zero IDs are dropped, never emitted.
#[must_use]
pub fn companion_overwrites(plan: &TextChannelPlan, bot_id: Snowflake) -> Vec<PermissionOverwrite> {
    let mut overwrites: Vec<PermissionOverwrite> = plan
        .overwrites
        .iter()
        .filter_map(|overwrite| wire_overwrite(plan.guild_id, overwrite))
        .collect();
    if bot_id != 0
        && !overwrites.iter().any(|overwrite| {
            overwrite.kind == PermissionOverwriteType::Member && overwrite.id.get() == bot_id
        })
    {
        overwrites.push(PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL,
            deny: Permissions::empty(),
            id: Id::new(bot_id),
            kind: PermissionOverwriteType::Member,
        });
    }
    overwrites
}

/// One occupant's View grant on the companion (V9 join): Member allow, never
/// a deny.
#[must_use]
pub fn companion_view_grant(member_id: Snowflake) -> Option<PermissionOverwrite> {
    if member_id == 0 {
        return None;
    }
    Some(PermissionOverwrite {
        allow: Permissions::VIEW_CHANNEL,
        deny: Permissions::empty(),
        id: Id::new(member_id),
        kind: PermissionOverwriteType::Member,
    })
}

/// Sanitized errors: never surface HTTP bodies/tokens/member data in diagnostics.
/// Upper bound for one rename request (admission plus Discord round trip).
pub const RENAME_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound for one voice-status request (send admission plus the
/// Discord round trip).
pub const VOICE_STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Discord's voice channel status limit.
pub const MAX_VOICE_STATUS_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RoomHttpError {
    #[error("Discord rate limit; retry after {retry_after_ms}ms")]
    RateLimited { retry_after_ms: u64, global: bool },
    #[error("Discord access denied; suspend until permissions change")]
    AccessDenied,
    #[error("Discord channel or member no longer exists")]
    NotFound,
    #[error("Discord credential refused; stop the worker")]
    Unauthorized,
    #[error("voice state changed while waiting; cancel this write")]
    Cancelled,
    #[error("rename deferred; release the guild lane and keep only its latest name")]
    RenameDeferred,
    #[error("invalid Discord room request")]
    InvalidRequest,
    #[error("Discord rejected room request (HTTP {status}, code {code})")]
    Rejected { status: u16, code: u64 },
    #[error("Discord write outcome unknown; do not recreate the channel")]
    UnknownOutcome,
}

/// Accept only a channel matching the create request. A malformed or
/// contradictory 2xx may still have created a channel: never turn it into
/// proof of no effect, expose the body, or derive ownership from its id.
fn creation_receipt(
    body: &[u8],
    guild_id: Snowflake,
    kind: ChannelType,
    parent_id: Option<Snowflake>,
) -> Result<Channel, RoomHttpError> {
    // Twilight's Id deserializer rejects missing, malformed and zero ids.
    let channel: Channel =
        serde_json::from_slice(body).map_err(|_| RoomHttpError::UnknownOutcome)?;
    if channel.guild_id.map(Id::get) != Some(guild_id)
        || channel.kind != kind
        || channel.parent_id.map(Id::get) != parent_id
    {
        return Err(RoomHttpError::UnknownOutcome);
    }
    Ok(channel)
}

/// A message the bot posted: the channel it landed in and its id, so the same
/// message can be edited later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageRef {
    pub channel_id: Snowflake,
    pub message_id: Snowflake,
}

/// A global rate-limit wait longer than this never parks the guild actor:
/// the call returns RateLimited so the action queue backs off tick-driven
/// (visible in gauges) instead of sleeping silently inside the worker loop.
const GLOBAL_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Upward spread for one sub-cap global-429 wait: the shared deadline plus
/// up to this much, hard-capped at [`GLOBAL_BACKOFF_MAX`].
const GLOBAL_BACKOFF_SPREAD: Duration = Duration::from_secs(5);

/// Deterministic 64-bit mixer (splitmix64) for seeded jitter without a
/// global RNG: the same `seed` always yields the same draw, on every
/// platform, with no clock and no `rand` dependency.
fn jitter_draw(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Total sleep for a sub-cap global-429 `wait` (the remaining shared-deadline
/// wait, at most `GLOBAL_BACKOFF_MAX`): the deadline plus a deterministic
/// upward spread in `[wait, (wait + GLOBAL_BACKOFF_SPREAD).min(GLOBAL_BACKOFF_MAX)]`,
/// so guild actors sharing one global limit stop waking on the same instant.
/// Never below `wait` (waking early only earns another 429) and never above
/// the cap, so the cap and the `RateLimited` queue-handoff behavior hold by
/// construction. A zero wait stays zero (no deadline, no sleep); at exactly
/// the cap the spread window is empty and the sleep equals the wait. The same
/// `seed` replays the same sleep, so tests pin ranges without a clock or RNG;
/// production passes `two_bot_core::action_outcomes::jitter_seed()`.
#[must_use]
pub fn jitter_sub_cap_wait(wait: Duration, seed: u64) -> Duration {
    if wait.is_zero() {
        return Duration::ZERO;
    }
    let ceiling = wait
        .saturating_add(GLOBAL_BACKOFF_SPREAD)
        .min(GLOBAL_BACKOFF_MAX);
    let window_nanos = ceiling.saturating_sub(wait).as_nanos();
    // `span` is at least 1, so the remainder below never divides by zero.
    let span = window_nanos.saturating_add(1).min(u128::from(u64::MAX)) as u64;
    wait.saturating_add(Duration::from_nanos(jitter_draw(seed) % span))
}

#[cfg(test)]
mod jitter_sub_cap_wait_tests {
    use super::{jitter_sub_cap_wait, Duration, GLOBAL_BACKOFF_MAX, GLOBAL_BACKOFF_SPREAD};

    #[test]
    fn sub_cap_sleep_covers_the_deadline_with_bounded_spread() {
        for secs in [1u64, 10, 25, 29] {
            let wait = Duration::from_secs(secs);
            let ceiling = (wait + GLOBAL_BACKOFF_SPREAD).min(GLOBAL_BACKOFF_MAX);
            for seed in 0..8u64 {
                let sleep = jitter_sub_cap_wait(wait, seed);
                assert!(
                    sleep >= wait && sleep <= ceiling,
                    "wait {wait:?} seed {seed}: {sleep:?} outside [{wait:?}, {ceiling:?}]"
                );
            }
        }
    }

    #[test]
    fn zero_and_capped_waits_are_unjittered() {
        assert_eq!(jitter_sub_cap_wait(Duration::ZERO, 42), Duration::ZERO);
        assert_eq!(
            jitter_sub_cap_wait(GLOBAL_BACKOFF_MAX, 42),
            GLOBAL_BACKOFF_MAX
        );
    }

    #[test]
    fn sub_cap_sleep_spreads_wake_times_across_seeds() {
        use std::collections::HashSet;
        // A 10 s shared wait wakes every guild actor on the same instant today.
        let distinct: HashSet<Duration> = (0..32u64)
            .map(|seed| jitter_sub_cap_wait(Duration::from_secs(10), seed))
            .collect();
        assert!(
            distinct.len() >= 8,
            "only {} distinct wake times across 32 seeds",
            distinct.len()
        );
    }
}

/// In-flight voice REST attempts, for the in-flight-age gauge.
#[derive(Debug, Default)]
struct InFlightRest {
    next_id: u64,
    live: std::collections::HashMap<u64, Instant>,
}

impl InFlightRest {
    fn begin(slots: &Arc<Mutex<Self>>) -> InFlightGuard {
        let (id, slots) = {
            let mut inner = slots.lock().expect("in-flight lock");
            inner.next_id += 1;
            let id = inner.next_id;
            inner.live.insert(id, Instant::now());
            (id, Arc::clone(slots))
        };
        publish_rest_age(&slots);
        InFlightGuard {
            slots: Some(slots),
            id,
        }
    }
}

/// Oldest live REST attempt in whole seconds. Pure computation behind
/// [`publish_rest_age`]: tests assert on this return value instead of the
/// process-global gauge, which is last-writer-wins and flakes when another
/// test's begin/drop lands between the publish and the read.
fn oldest_age_secs(slots: &Arc<Mutex<InFlightRest>>) -> u64 {
    // Silent recovery: the `InFlightGuard` drop body owns the poison
    // counter emission, and this only observes a poisoned lock through
    // that drop path (`begin` locks before publishing, so it never
    // reaches here on a poisoned lock).
    slots
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .live
        .values()
        .map(|started| started.elapsed().as_secs())
        .max()
        .unwrap_or(0)
}

fn publish_rest_age(slots: &Arc<Mutex<InFlightRest>>) {
    let age_secs = oldest_age_secs(slots);
    two_bot_core::metrics::global().set_voice_rest_in_flight_age_secs(age_secs);
}

/// Removes one in-flight attempt and republishes the gauge on drop, so
/// timeouts and early returns cannot leak the slot.
struct InFlightGuard {
    slots: Option<Arc<Mutex<InFlightRest>>>,
    id: u64,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Some(slots) = self.slots.take() {
            two_bot_core::metrics::recover_drop_poison(slots.lock(), "in_flight_guard")
                .live
                .remove(&self.id);
            publish_rest_age(&slots);
        }
    }
}

#[derive(Clone)]
pub struct RoomHttp {
    // Twilight builds and validates requests, but its ResponseFuture retries
    // 429s indefinitely. A single-attempt transport returns retry-after to the
    // action queue, so a rename cannot hold the guild's lifecycle lane.
    http: Arc<Client>,
    transport: HyperClient<HttpsConnector<HttpConnector>, Full<Bytes>>,
    authorization: HeaderValue,
    origin: String,
    // Scheme/authority used for attachment downloads after the user URL has
    // been validated against the CDN allowlist. Production is the Discord
    // CDN itself, so the rewrite is the identity; tests point it at a
    // loopback server, mirroring the `origin` seam for API requests.
    cdn_origin: String,
    global_not_before: Arc<Mutex<Option<Instant>>>,
    unauthorized: Arc<AtomicBool>,
    in_flight: Arc<Mutex<InFlightRest>>,
}

impl std::fmt::Debug for RoomHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomHttp").finish_non_exhaustive()
    }
}

impl RoomHttp {
    pub fn new(token: String) -> Result<Self, RoomHttpError> {
        Self::with_origin(token, "https://discord.com/api/v10".to_owned(), false)
    }

    /// Whether a 401 permanently latched this writer: every later write
    /// refuses immediately until a restart rebuilds the client, so a healthy
    /// store re-probe must never clear a halt this latch caused.
    pub fn is_unauthorized(&self) -> bool {
        self.unauthorized.load(Ordering::Relaxed)
    }

    /// Recompute the in-flight-age gauge from attempts still live, so a
    /// per-tick publish shows a currently-stalled request instead of the age
    /// at begin (always 0) or after drop (already removed).
    pub fn republish_rest_age(&self) {
        publish_rest_age(&self.in_flight);
    }

    fn with_origin(token: String, origin: String, allow_http: bool) -> Result<Self, RoomHttpError> {
        let mut authorization = HeaderValue::from_str(&format!("Bot {token}"))
            .map_err(|_| RoomHttpError::InvalidRequest)?;
        authorization.set_sensitive(true);
        let connector = HttpsConnectorBuilder::new().with_webpki_roots();
        let connector = if allow_http {
            connector.https_or_http()
        } else {
            connector.https_only()
        }
        .enable_http1()
        .build();
        // Source: https://docs.rs/hyper-util/0.1.20/hyper_util/client/legacy/struct.Builder.html#method.retry_canceled_requests
        let transport = HyperClient::builder(TokioExecutor::new())
            .retry_canceled_requests(false)
            .build(connector);
        Ok(Self {
            http: Arc::new(Client::builder().build()),
            transport,
            authorization,
            origin,
            cdn_origin: "https://cdn.discordapp.com".to_owned(),
            global_not_before: Arc::new(Mutex::new(None)),
            unauthorized: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::new(Mutex::new(InFlightRest::default())),
        })
    }

    async fn send(
        &self,
        request: twilight_http::request::Request,
        still_valid: impl Fn() -> bool + Send,
    ) -> Result<Bytes, RoomHttpError> {
        self.send_request(request, still_valid, true).await
    }

    async fn send_request(
        &self,
        request: twilight_http::request::Request,
        still_valid: impl Fn() -> bool + Send,
        bot_authenticated: bool,
    ) -> Result<Bytes, RoomHttpError> {
        // Interaction callbacks/webhooks use their own token, not the bot
        // credential or its global rate limit. Never log their request paths.
        // A stale join must not sleep out a backoff it will never use.
        if !still_valid() {
            return Err(RoomHttpError::Cancelled);
        }
        loop {
            if !bot_authenticated {
                break;
            }
            if self.unauthorized.load(Ordering::Relaxed) {
                return Err(RoomHttpError::Unauthorized);
            }
            let until = *self.global_not_before.lock().expect("global backoff lock");
            match until {
                Some(until) if until > Instant::now() => {
                    let wait = until.saturating_duration_since(Instant::now());
                    if wait > GLOBAL_BACKOFF_MAX {
                        // Never park the guild actor on a far-future global
                        // limit: hand the remaining wait to the action queue,
                        // which backs off tick-driven (gauges stay live)
                        // instead of sleeping silently in the worker loop.
                        two_bot_core::metrics::global().voice_global_backoff_capped();
                        tracing::warn!(
                            wait_secs = wait.as_secs(),
                            "voice global backoff capped; returning RateLimited for queue backoff"
                        );
                        return Err(RoomHttpError::RateLimited {
                            retry_after_ms: wait.as_millis().min(u128::from(u64::MAX)) as u64,
                            global: true,
                        });
                    }
                    if wait > Duration::from_secs(1) {
                        tracing::info!(wait_secs = wait.as_secs(), "voice global backoff wait");
                    }
                    // Spread the shared-deadline wake: every guild actor
                    // sleeping until `until` wakes on the same instant today.
                    let seed = two_bot_core::action_outcomes::jitter_seed();
                    tokio::time::sleep(jitter_sub_cap_wait(wait, seed)).await;
                    if !still_valid() {
                        return Err(RoomHttpError::Cancelled);
                    }
                }
                _ => break,
            }
        }
        // Attachment-bearing Twilight requests carry a multipart form, not a
        // JSON body: send the built form with its declared boundary, the
        // same conversion the upstream client performs. JSON-only requests
        // keep the previous content type and body. Request-supplied headers
        // still apply last, after this default.
        let (content_type, body) = match request.form() {
            Some(form) => {
                let content_type = HeaderValue::from_bytes(&form.content_type())
                    .map_err(|_| RoomHttpError::InvalidRequest)?;
                (content_type, Bytes::from(form.clone().build()))
            }
            None => (
                HeaderValue::from_static("application/json"),
                Bytes::copy_from_slice(request.body().unwrap_or_default()),
            ),
        };
        let mut wire = Request::builder()
            .method(request.method().name())
            .uri(format!("{}/{}", self.origin, request.path()))
            .header(CONTENT_TYPE, content_type);
        if bot_authenticated {
            wire = wire.header(AUTHORIZATION, self.authorization.clone());
        }
        if let Some(headers) = request.headers() {
            for (name, value) in headers {
                wire = wire.header(name, value);
            }
        }
        let wire = wire
            .body(Full::new(body))
            .map_err(|_| RoomHttpError::InvalidRequest)?;
        // One adapter-visible REST attempt per wire send, shared with the
        // executor's accounting: template only, never IDs, tokens or bodies.
        // `Attempt` finishes at headers so 429s stay visible even when the
        // body is broken; transport failures and outer timeouts fall back to
        // `transport` via `Drop`.
        let mut attempt = crate::executor_metrics::Attempt::new(&request);
        // Tracked for the in-flight-age gauge; the guard removes the slot on
        // drop so timeouts cannot leak it.
        let _in_flight = InFlightRest::begin(&self.in_flight);
        let response = tokio::time::timeout(Duration::from_secs(10), async {
            let response = self
                .transport
                .request(wire)
                .await
                .map_err(|_| RoomHttpError::UnknownOutcome)?;
            let status = response.status().as_u16();
            attempt.finish(Some(status));
            if status == 401 {
                if bot_authenticated {
                    self.unauthorized.store(true, Ordering::Relaxed);
                }
                return Err(RoomHttpError::Unauthorized);
            }
            let header_delay = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after_ms);
            let global_header = response
                .headers()
                .get("x-ratelimit-scope")
                .is_some_and(|value| value == "global");
            let body = Limited::new(response.into_body(), 1_048_576)
                .collect()
                .await
                .map_err(|_| RoomHttpError::UnknownOutcome)?
                .to_bytes();
            if (200..300).contains(&status) {
                return Ok(body);
            }
            let mut error = classify_response(status, &body);
            if let RoomHttpError::RateLimited {
                retry_after_ms,
                global,
            } = &mut error
            {
                *retry_after_ms = (*retry_after_ms).max(header_delay.unwrap_or(0));
                *global |= global_header;
                if *global && bot_authenticated {
                    let until = Instant::now()
                        .checked_add(Duration::from_millis(*retry_after_ms))
                        .ok_or(RoomHttpError::InvalidRequest)?;
                    let mut stored = self.global_not_before.lock().expect("global backoff lock");
                    *stored = Some(stored.map_or(until, |previous| previous.max(until)));
                }
            }
            Err(error)
        })
        .await
        .map_err(|_| RoomHttpError::UnknownOutcome)?;
        response
    }

    /// One callback attempt, bounded below Discord's three-second deadline.
    /// No bot authorization, global bot backoff, or internal 429 retries.
    pub async fn respond_interaction(
        &self,
        application: Id<ApplicationMarker>,
        interaction: Id<InteractionMarker>,
        token: &str,
        response: &InteractionResponse,
    ) -> Result<(), RoomHttpError> {
        let request = self
            .http
            .interaction(application)
            .create_response(interaction, token, response)
            .try_into_request()
            .map_err(classify_http_error)?;
        tokio::time::timeout(
            Duration::from_millis(2500),
            self.send_request(request, || true, false),
        )
        .await
        .map_err(|_| RoomHttpError::UnknownOutcome)??;
        Ok(())
    }

    /// Read the original callback message without retaining the interaction
    /// token. One bounded GET, no bot authentication or callback retry.
    pub async fn interaction_message(
        &self,
        application: Id<ApplicationMarker>,
        token: &str,
    ) -> Result<MessageRef, RoomHttpError> {
        let request = self
            .http
            .interaction(application)
            .response(token)
            .try_into_request()
            .map_err(classify_http_error)?;
        let body = self.send_request(request, || true, false).await?;
        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|_| RoomHttpError::InvalidRequest)?;
        let channel_id = value
            .get("channel_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| id.parse::<u64>().ok())
            .filter(|id| *id != 0)
            .ok_or(RoomHttpError::InvalidRequest)?;
        parse_posted_message(channel_id, &body)
    }

    /// Complete the deferred response, never a second initial response.
    /// `attachments` carries `/export` file bytes; `components` carries the
    /// `/import` Confirm/Cancel buttons. Both are omitted when empty, so a
    /// plain text edit changes nothing else on the message.
    pub async fn complete_interaction(
        &self,
        application: Id<ApplicationMarker>,
        token: &str,
        content: &str,
        attachments: &[Attachment],
        components: Option<&[Component]>,
    ) -> Result<(), RoomHttpError> {
        let mentions = AllowedMentions::default();
        let interaction = self.http.interaction(application);
        let mut update = interaction
            .update_response(token)
            .content(Some(content))
            .allowed_mentions(Some(&mentions));
        if !attachments.is_empty() {
            update = update.attachments(attachments);
        }
        if let Some(components) = components {
            update = update.components(Some(components));
        }
        let request = update.try_into_request().map_err(classify_http_error)?;
        self.send_request(request, || true, false).await?;
        Ok(())
    }

    /// Download a Discord-hosted `/import` file, capped at `max_bytes`.
    /// Only the Discord CDN host is fetched, over HTTPS and without the bot
    /// credential: attachment URLs are signed and need no authorization.
    ///
    /// One ten-second elapsed-time budget covers the request, the status
    /// check and collection of the whole body together, measured from the
    /// start of the download; it is never restarted when headers arrive, so
    /// a response that returns valid headers and then stalls or drips
    /// chunks still fails at the deadline. The `max_bytes` cap bounds size
    /// independently: a body of exactly `max_bytes` succeeds, one byte more
    /// is refused. A deadline expiry, an oversized body, a non-2xx status
    /// (redirects are classified, never followed) and transport failures
    /// all surface as errors with no partial bytes. Failures never echo the
    /// signed attachment URL. The request is marked `connection: close`:
    /// a non-2xx body is deliberately never read, and this one-shot
    /// download must not return a connection carrying that unread (and
    /// possibly stalled) body to the shared pool for a later request.
    pub async fn download_attachment(
        &self,
        url: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, RoomHttpError> {
        let uri: http::Uri = url.parse().map_err(|_| RoomHttpError::InvalidRequest)?;
        if uri.scheme() != Some(&http::uri::Scheme::HTTPS)
            || uri.host() != Some("cdn.discordapp.com")
        {
            return Err(RoomHttpError::InvalidRequest);
        }
        // The user URL is validated above exactly as supplied; only the
        // transport target's scheme/authority come from `cdn_origin`
        // (identity in production), preserving path and signed query.
        let path_and_query = uri
            .path_and_query()
            .map_or("/", http::uri::PathAndQuery::as_str);
        let transport_uri: http::Uri =
            format!("{}{path_and_query}", self.cdn_origin.trim_end_matches('/'))
                .parse()
                .map_err(|_| RoomHttpError::InvalidRequest)?;
        let request = Request::builder()
            .method("GET")
            .uri(transport_uri)
            .header(http::header::CONNECTION, "close")
            .body(Full::new(Bytes::new()))
            .map_err(|_| RoomHttpError::InvalidRequest)?;
        let download = async {
            let response = self
                .transport
                .request(request)
                .await
                .map_err(|_| RoomHttpError::UnknownOutcome)?;
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                return Err(classify_response(status, &[]));
            }
            let body = Limited::new(response.into_body(), max_bytes.saturating_add(1))
                .collect()
                .await
                .map_err(|_| RoomHttpError::UnknownOutcome)?
                .to_bytes();
            if body.len() > max_bytes {
                return Err(RoomHttpError::InvalidRequest);
            }
            Ok(body.to_vec())
        };
        tokio::time::timeout(Duration::from_secs(10), download)
            .await
            .map_err(|_| RoomHttpError::UnknownOutcome)?
    }

    pub async fn create_room(
        &self,
        guild_id: Snowflake,
        name: &str,
        attributes: &RoomChannelAttributes,
        still_in_creator: impl Fn() -> bool + Send + 'static,
    ) -> Result<Channel, RoomHttpError> {
        if guild_id == 0 || attributes.parent_id == Some(0) || attributes.user_limit > 99 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let mut request = self
            .http
            .create_guild_channel(Id::new(guild_id), name)
            .kind(ChannelType::GuildVoice)
            .nsfw(attributes.nsfw)
            .user_limit(attributes.user_limit);
        // An empty list is omitted rather than sent, so a category-synced room
        // really syncs instead of being created with an explicit empty set.
        if !attributes.overwrites.is_empty() {
            request = request.permission_overwrites(&attributes.overwrites);
        }
        if let Some(position) = attributes.position {
            request = request.position(position);
        }
        if let Some(parent_id) = attributes.parent_id {
            request = request.parent_id(Id::new(parent_id));
        }
        if let Some(bitrate) = attributes.bitrate {
            request = request.bitrate(bitrate);
        }
        if let Some(region) = &attributes.rtc_region {
            request = request.rtc_region(region);
        }
        if let Some(quality) = attributes.video_quality_mode {
            request = request.video_quality_mode(quality);
        }
        let request = request.try_into_request().map_err(classify_http_error)?;
        let body = self.send(request, still_in_creator).await?;
        creation_receipt(
            &body,
            guild_id,
            ChannelType::GuildVoice,
            attributes.parent_id,
        )
    }

    /// Discord's bulk `PATCH /guilds/{guild.id}/channels`: set each listed
    /// channel's position in one request (Manage Channels).
    pub async fn reorder_channels(
        &self,
        guild_id: Snowflake,
        positions: &[(Snowflake, u64)],
    ) -> Result<(), RoomHttpError> {
        if guild_id == 0 || positions.iter().any(|(id, _)| *id == 0) {
            return Err(RoomHttpError::InvalidRequest);
        }
        let body: Vec<serde_json::Value> = positions
            .iter()
            .map(|(id, position)| serde_json::json!({ "id": id.to_string(), "position": position }))
            .collect();
        let request = twilight_http::request::RequestBuilder::raw(
            twilight_http::request::Method::Patch,
            format!("guilds/{guild_id}/channels"),
        )
        .json(&body)
        .build()
        .map_err(|_| RoomHttpError::InvalidRequest)?;
        self.send(request, || true).await?;
        Ok(())
    }

    // Source: https://docs.rs/twilight-http/0.17.1/twilight_http/request/guild/member/struct.UpdateGuildMember.html
    pub async fn move_member(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        channel_id: Snowflake,
        still_in_creator: impl Fn() -> bool + Send + 'static,
    ) -> Result<(), RoomHttpError> {
        if guild_id == 0 || member_id == 0 || channel_id == 0 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let request = self
            .http
            .update_guild_member(Id::new(guild_id), Id::new(member_id))
            .channel_id(Some(Id::new(channel_id)))
            .try_into_request()
            .map_err(classify_http_error)?;
        self.send(request, still_in_creator).await?;
        Ok(())
    }

    /// V4 vote-kick enforcement, first half: move the member out of voice by
    /// clearing their voice channel (`channel_id: null`). Source:
    /// <https://docs.rs/twilight-http/0.17.1/twilight_http/request/guild/member/struct.UpdateGuildMember.html>
    /// A 404 means the member already left: the spec cancels the vote when the
    /// target leaves, so treat it as success.
    pub async fn disconnect_member(
        &self,
        guild_id: Snowflake,
        member_id: Snowflake,
        still_valid: impl Fn() -> bool + Send + 'static,
    ) -> Result<(), RoomHttpError> {
        if guild_id == 0 || member_id == 0 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let request = self
            .http
            .update_guild_member(Id::new(guild_id), Id::new(member_id))
            .channel_id(None)
            .try_into_request()
            .map_err(classify_http_error)?;
        match self.send(request, still_valid).await {
            Ok(_) => Ok(()),
            Err(error) => match error {
                // A duplicate disconnect is success: the member already left.
                RoomHttpError::NotFound => Ok(()),
                other => Err(other),
            },
        }
    }

    /// V4 vote-kick enforcement, second half: deny Connect to the target on
    /// this room channel only (member-scoped overwrite, not a guild kick or
    /// ban). CONNECT is denied while every other bit is left alone:
    /// allow carries empty so the write neither grants nor (via `allow`)
    /// preserves anything outside the deny bit.
    ///
    /// Named `deny_member_connect` (not `deny_connect`) so the `RoomWrites`
    /// trait impl can call it without resolving to itself.
    pub async fn deny_member_connect(
        &self,
        channel_id: Snowflake,
        member_id: Snowflake,
        still_valid: impl Fn() -> bool + Send + 'static,
    ) -> Result<(), RoomHttpError> {
        if channel_id == 0 || member_id == 0 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let overwrite = HttpPermissionOverwrite {
            allow: Some(Permissions::empty()),
            deny: Some(Permissions::CONNECT),
            id: Id::new(member_id),
            kind: HttpPermissionOverwriteType::Member,
        };
        let request = self
            .http
            .update_channel_permission(Id::new(channel_id), &overwrite)
            .try_into_request()
            .map_err(classify_http_error)?;
        self.send(request, still_valid).await?;
        Ok(())
    }

    /// The worker must only supply channels from its tracked-room store or from
    /// its own successful create result (compensation). Never use a category scan.
    pub async fn delete_room(
        &self,
        channel_id: Snowflake,
        may_delete: impl Fn() -> bool + Send + 'static,
    ) -> Result<(), RoomHttpError> {
        if channel_id == 0 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let request = self
            .http
            .delete_channel(Id::new(channel_id))
            .try_into_request()
            .map_err(classify_http_error)?;
        match self.send(request, may_delete).await {
            Ok(_) => Ok(()),
            Err(error) => match error {
                // A duplicate delete is success; 403 is NOT proof of deletion.
                RoomHttpError::NotFound => Ok(()),
                other => Err(other),
            },
        }
    }

    /// Create a V9 companion text channel in the room's category, with the
    /// full overwrite set (including the bot's View allow) in the POST —
    /// never patched afterwards. One POST per invocation; receipt validation
    /// failures are uncertain outcomes. The worker's snapshot adoption and
    /// later queue retries are separate from this adapter's single attempt.
    pub async fn create_companion(
        &self,
        plan: &TextChannelPlan,
        bot_id: Snowflake,
        still_managing: impl Fn() -> bool + Send + 'static,
    ) -> Result<Channel, RoomHttpError> {
        if plan.guild_id == 0 || plan.room_id == 0 || plan.category_id == 0 || bot_id == 0 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let request = self
            .http
            .create_guild_channel(Id::new(plan.guild_id), &plan.name)
            .kind(ChannelType::GuildText)
            .parent_id(Id::new(plan.category_id))
            .permission_overwrites(&companion_overwrites(plan, bot_id))
            .try_into_request()
            .map_err(classify_http_error)?;
        let body = self.send(request, still_managing).await?;
        creation_receipt(
            &body,
            plan.guild_id,
            ChannelType::GuildText,
            Some(plan.category_id),
        )
    }

    /// Grant one occupant View on the companion (V9 join): a Member allow,
    /// never a deny.
    pub async fn grant_companion_view(
        &self,
        text_channel_id: Snowflake,
        member_id: Snowflake,
        still_managing: impl Fn() -> bool + Send + 'static,
    ) -> Result<(), RoomHttpError> {
        let overwrite = companion_view_grant(member_id).ok_or(RoomHttpError::InvalidRequest)?;
        if text_channel_id == 0 {
            return Err(RoomHttpError::InvalidRequest);
        }
        // The edit endpoint takes the outbound model, not the channel model.
        let wire = HttpPermissionOverwrite {
            allow: Some(overwrite.allow),
            deny: Some(overwrite.deny),
            id: overwrite.id,
            kind: HttpPermissionOverwriteType::Member,
        };
        let request = self
            .http
            .update_channel_permission(Id::new(text_channel_id), &wire)
            .try_into_request()
            .map_err(classify_http_error)?;
        self.send(request, still_managing).await?;
        Ok(())
    }

    /// Remove one occupant's overwrite from the companion (V9 leave): the
    /// overwrite is deleted, never replaced with a deny. Deleting an absent
    /// overwrite is success.
    pub async fn revoke_companion_view(
        &self,
        text_channel_id: Snowflake,
        member_id: Snowflake,
        still_managing: impl Fn() -> bool + Send + 'static,
    ) -> Result<(), RoomHttpError> {
        self.delete_member_overwrite(text_channel_id, member_id, still_managing)
            .await
    }

    /// Delete one member's overwrite on a channel. Deleting an absent
    /// overwrite (or a channel already gone) is success: the end state — no
    /// overwrite — already holds.
    pub async fn delete_member_overwrite(
        &self,
        channel_id: Snowflake,
        member_id: Snowflake,
        still_valid: impl Fn() -> bool + Send + 'static,
    ) -> Result<(), RoomHttpError> {
        if channel_id == 0 || member_id == 0 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let request = self
            .http
            .delete_channel_permission(Id::new(channel_id))
            .member(Id::new(member_id))
            .try_into_request()
            .map_err(classify_http_error)?;
        match self.send(request, still_valid).await {
            Ok(_) => Ok(()),
            Err(RoomHttpError::NotFound) => Ok(()),
            Err(other) => Err(other),
        }
    }

    /// V3 `/private` and `/public`: replace one role or member overwrite on a
    /// room channel. The write is a PUT that replaces the target's whole
    /// overwrite, so the caller passes the bits it wants kept. Manage Roles is
    /// never written as an allow, whatever the caller passes (the create-time
    /// exclusion holds for every overwrite this adapter emits), and CONNECT or
    /// any other bit may not sit in both lists.
    pub async fn put_channel_overwrite(
        &self,
        channel_id: Snowflake,
        overwrite: &PermissionOverwrite,
        still_valid: impl Fn() -> bool + Send + 'static,
    ) -> Result<(), RoomHttpError> {
        if channel_id == 0 || overwrite.id.get() == 0 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let kind = match overwrite.kind {
            PermissionOverwriteType::Role => HttpPermissionOverwriteType::Role,
            PermissionOverwriteType::Member => HttpPermissionOverwriteType::Member,
            _ => return Err(RoomHttpError::InvalidRequest),
        };
        let deny = overwrite.deny;
        let allow = (overwrite.allow & !Permissions::MANAGE_ROLES) & !deny;
        let wire = HttpPermissionOverwrite {
            allow: Some(allow),
            deny: Some(deny),
            id: overwrite.id,
            kind,
        };
        let request = self
            .http
            .update_channel_permission(Id::new(channel_id), &wire)
            .try_into_request()
            .map_err(classify_http_error)?;
        self.send(request, still_valid).await?;
        Ok(())
    }

    /// V3: create the Join voice channel next to a private room. No
    /// overwrites ride the POST, so it syncs to the category and the bot's own
    /// access comes from the category rather than a new grant. One POST per
    /// invocation; invalid receipts are uncertain outcomes. This does not fence
    /// the worker's separate retry/adoption path (see the creation-plan doc).
    pub async fn create_join_voice_channel(
        &self,
        guild_id: Snowflake,
        name: &str,
        parent_id: Option<Snowflake>,
        position: Option<u64>,
        still_valid: impl Fn() -> bool + Send + 'static,
    ) -> Result<Channel, RoomHttpError> {
        if guild_id == 0 || parent_id == Some(0) {
            return Err(RoomHttpError::InvalidRequest);
        }
        let mut request = self
            .http
            .create_guild_channel(Id::new(guild_id), name)
            .kind(ChannelType::GuildVoice);
        if let Some(position) = position {
            request = request.position(position);
        }
        if let Some(parent_id) = parent_id {
            request = request.parent_id(Id::new(parent_id));
        }
        let request = request.try_into_request().map_err(classify_http_error)?;
        let body = self.send(request, still_valid).await?;
        creation_receipt(&body, guild_id, ChannelType::GuildVoice, parent_id)
    }

    /// Post an operator notice to a channel. Only the given role (if any) can
    /// be pinged: `allowed_mentions` is otherwise empty, so notice text can
    /// never ping members, `@everyone` or other roles.
    pub async fn post_notice(
        &self,
        channel_id: Snowflake,
        content: &str,
        mention_role: Option<Snowflake>,
    ) -> Result<(), RoomHttpError> {
        if channel_id == 0 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let mut text = String::new();
        let mut mentions = AllowedMentions::default();
        if let Some(role) = mention_role.filter(|role| *role != 0) {
            text.push_str(&format!("<@&{role}> "));
            mentions.roles.push(Id::new(role));
        }
        text.push_str(content);
        let request = self
            .http
            .create_message(Id::new(channel_id))
            .content(&text)
            .allowed_mentions(Some(&mentions))
            .try_into_request()
            .map_err(classify_http_error)?;
        self.send(request, || true).await?;
        Ok(())
    }

    /// Open (or reuse) the DM channel with `user_id`, then post the notice
    /// with no mentions allowed.
    pub async fn direct_notice(
        &self,
        user_id: Snowflake,
        content: &str,
    ) -> Result<(), RoomHttpError> {
        if user_id == 0 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let request = self
            .http
            .create_private_channel(Id::new(user_id))
            .try_into_request()
            .map_err(classify_http_error)?;
        let body = self.send(request, || true).await?;
        let channel: Channel =
            serde_json::from_slice(&body).map_err(|_| RoomHttpError::UnknownOutcome)?;
        self.post_notice(channel.id.get(), content, None).await
    }

    /// V3 join request: post a message with buttons to a channel and return
    /// where it landed. Only `mention_user` (if any) can be pinged:
    /// `allowed_mentions` is otherwise empty, so any other mention in
    /// `content` renders without a ping. Single attempt, so an unknown
    /// outcome never posts a second prompt on its own.
    pub async fn post_component_message(
        &self,
        channel_id: Snowflake,
        content: &str,
        mention_user: Option<Snowflake>,
        components: &[Component],
    ) -> Result<MessageRef, RoomHttpError> {
        if channel_id == 0 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let mut mentions = AllowedMentions::default();
        if let Some(user) = mention_user.filter(|user| *user != 0) {
            mentions.users.push(Id::new(user));
        }
        let request = self
            .http
            .create_message(Id::new(channel_id))
            .content(content)
            .components(components)
            .allowed_mentions(Some(&mentions))
            .try_into_request()
            .map_err(classify_http_error)?;
        let body = self.send(request, || true).await?;
        parse_posted_message(channel_id, &body)
    }

    /// Replace the text and buttons of a message the bot posted. An empty
    /// `components` list removes the buttons. No mentions are allowed. A
    /// message (or its channel) that is already gone is `NotFound`.
    pub async fn edit_component_message(
        &self,
        message: MessageRef,
        content: &str,
        components: &[Component],
    ) -> Result<(), RoomHttpError> {
        if message.channel_id == 0 || message.message_id == 0 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let mentions = AllowedMentions::default();
        let request = self
            .http
            .update_message(Id::new(message.channel_id), Id::new(message.message_id))
            .content(Some(content))
            .components(Some(components))
            .allowed_mentions(Some(&mentions))
            .try_into_request()
            .map_err(classify_http_error)?;
        self.send(request, || true).await?;
        Ok(())
    }

    /// Set (or clear with `""`) a voice channel's status line: Discord's
    /// `PUT /channels/{channel.id}/voice-status`, which needs Set Voice Channel
    /// Status. Bounded like renames; the caller keeps only the latest text.
    pub async fn set_room_voice_status(
        &self,
        channel_id: Snowflake,
        status: &str,
    ) -> Result<(), RoomHttpError> {
        if channel_id == 0 || status.chars().count() > MAX_VOICE_STATUS_CHARS {
            return Err(RoomHttpError::InvalidRequest);
        }
        let request = twilight_http::request::RequestBuilder::raw(
            twilight_http::request::Method::Put,
            format!("channels/{channel_id}/voice-status"),
        )
        .json(&serde_json::json!({ "status": status }))
        .build()
        .map_err(|_| RoomHttpError::InvalidRequest)?;
        tokio::time::timeout(VOICE_STATUS_REQUEST_TIMEOUT, self.send(request, || true))
            .await
            .map_err(|_| RoomHttpError::RenameDeferred)??;
        Ok(())
    }

    pub async fn rename_room(
        &self,
        channel_id: Snowflake,
        name: &str,
    ) -> Result<(), RoomHttpError> {
        if channel_id == 0 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let request = self
            .http
            .update_channel(Id::new(channel_id))
            .name(name)
            .try_into_request()
            .map_err(classify_http_error)?;
        // Bounded even for a stalled transport: rename budgets are charged on
        // attempt, including an unknown outcome. The queue keeps its latest name.
        // The bound covers durable send admission plus the Discord round trip,
        // which together exceed one second in production. The guild actor
        // waits at most one second of it; the rest runs on its own task.
        tokio::time::timeout(RENAME_REQUEST_TIMEOUT, self.send(request, || true))
            .await
            .map_err(|_| RoomHttpError::RenameDeferred)??;
        Ok(())
    }

    /// V3 `/limit` and `/unlimit`: set one room channel's user limit (`0` is
    /// unlimited). Discord accepts `0..=99`; anything larger is refused before
    /// the request is built. The write is idempotent, so a retry after an
    /// unknown outcome is safe.
    pub async fn set_room_user_limit(
        &self,
        channel_id: Snowflake,
        user_limit: u32,
        still_valid: impl Fn() -> bool + Send + 'static,
    ) -> Result<(), RoomHttpError> {
        let Ok(user_limit) = u16::try_from(user_limit) else {
            return Err(RoomHttpError::InvalidRequest);
        };
        if channel_id == 0 || user_limit > 99 {
            return Err(RoomHttpError::InvalidRequest);
        }
        let request = self
            .http
            .update_channel(Id::new(channel_id))
            .user_limit(user_limit)
            .try_into_request()
            .map_err(classify_http_error)?;
        self.send(request, still_valid).await?;
        Ok(())
    }
}

/// The id of a message Discord just created. A 2xx whose body does not name a
/// message is an unknown outcome, never an id of zero.
fn parse_posted_message(channel_id: Snowflake, body: &[u8]) -> Result<MessageRef, RoomHttpError> {
    #[derive(Deserialize)]
    struct Posted {
        id: String,
    }
    let posted: Posted = serde_json::from_slice(body).map_err(|_| RoomHttpError::UnknownOutcome)?;
    match posted.id.parse::<Snowflake>() {
        Ok(message_id) if message_id != 0 => Ok(MessageRef {
            channel_id,
            message_id,
        }),
        _ => Err(RoomHttpError::UnknownOutcome),
    }
}

fn classify_http_error(error: twilight_http::Error) -> RoomHttpError {
    classify_error_kind(error.kind())
}

fn classify_error_kind(kind: &ErrorType) -> RoomHttpError {
    match kind {
        ErrorType::Unauthorized => RoomHttpError::Unauthorized,
        ErrorType::RequestCanceled => RoomHttpError::Cancelled,
        ErrorType::Validation | ErrorType::BuildingRequest | ErrorType::Json => {
            RoomHttpError::InvalidRequest
        }
        ErrorType::Response { body, status, .. } => classify_response(status.get(), body),
        _ => RoomHttpError::UnknownOutcome,
    }
}

fn classify_response(status: u16, body: &[u8]) -> RoomHttpError {
    #[derive(Deserialize, Default)]
    struct ErrorBody {
        #[serde(default)]
        code: u64,
        #[serde(default)]
        retry_after: Option<serde_json::Number>,
        #[serde(default)]
        global: bool,
    }
    let body: ErrorBody = serde_json::from_slice(body).unwrap_or_default();
    match status {
        401 => RoomHttpError::Unauthorized,
        403 => RoomHttpError::AccessDenied,
        404 => RoomHttpError::NotFound,
        429 => RoomHttpError::RateLimited {
            retry_after_ms: body
                .retry_after
                .and_then(|value| parse_retry_after_ms(&value.to_string()))
                .unwrap_or(1_000)
                .max(1),
            global: body.global,
        },
        // A failed response can hide a successful non-idempotent create.
        500..=599 => RoomHttpError::UnknownOutcome,
        _ => RoomHttpError::Rejected {
            status,
            code: body.code,
        },
    }
}

#[cfg(test)]
#[path = "voice_rooms_http_tests.rs"]
mod http_tests;

#[cfg(test)]
#[path = "voice_import_deadline_tests.rs"]
mod voice_import_deadline_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn role(id: u64, permissions: Permissions) -> Role {
        serde_json::from_value(json!({
            "id": id.to_string(), "name": "role", "color": 0, "hoist": false,
            "managed": false, "mentionable": false, "position": 0,
            "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null },
            "permissions": permissions.bits().to_string(), "flags": 0
        }))
        .unwrap()
    }

    fn overwrite(
        id: u64,
        kind: PermissionOverwriteType,
        allow: Permissions,
        deny: Permissions,
    ) -> PermissionOverwrite {
        PermissionOverwrite {
            id: Id::new(id),
            kind,
            allow,
            deny,
        }
    }

    fn creator_channel() -> Channel {
        serde_json::from_value(json!({
            "id": "200", "guild_id": "100", "type": 2, "name": "creator",
            "parent_id": "400", "bitrate": 96000, "rtc_region": "rotterdam",
            "video_quality_mode": 2, "nsfw": true, "user_limit": 8
        }))
        .unwrap()
    }

    #[test]
    fn copies_attributes_and_distinguishes_inherit_from_unlimited() {
        let mut creator = CreatorChannel::new(100, 200);
        let channel = creator_channel();
        let overrides = vec![overwrite(
            100,
            PermissionOverwriteType::Role,
            Permissions::VIEW_CHANNEL,
            Permissions::empty(),
        )];
        let attributes =
            RoomChannelAttributes::from_creator(&creator, &channel, overrides.clone()).unwrap();
        assert_eq!(
            attributes,
            RoomChannelAttributes {
                parent_id: Some(400),
                bitrate: Some(96000),
                rtc_region: Some("rotterdam".to_owned()),
                video_quality_mode: Some(VideoQualityMode::Full),
                nsfw: true,
                user_limit: 8,
                position: None,
                respace: Vec::new(),
                fallback_position: None,
                overwrites: overrides,
            }
        );
        creator.default_limit = Some(0);
        assert_eq!(
            RoomChannelAttributes::from_creator(&creator, &channel, vec![])
                .unwrap()
                .user_limit,
            0
        );
        creator.default_limit = Some(4);
        assert_eq!(
            RoomChannelAttributes::from_creator(&creator, &channel, vec![])
                .unwrap()
                .user_limit,
            4
        );
        creator.guild_id = 101;
        assert_eq!(
            RoomChannelAttributes::from_creator(&creator, &channel, vec![]),
            Err(RoomHttpError::InvalidRequest)
        );
    }

    #[test]
    fn channel_denies_beat_guild_roles_and_member_overwrite_wins_last() {
        let roles = vec![
            role(100, Permissions::VIEW_CHANNEL | Permissions::CONNECT),
            role(
                500,
                Permissions::MANAGE_CHANNELS | Permissions::MOVE_MEMBERS,
            ),
        ];
        let deny = overwrite(
            100,
            PermissionOverwriteType::Role,
            Permissions::empty(),
            Permissions::CONNECT,
        );
        assert!(!can_manage_room(effective_permissions(
            100,
            999,
            300,
            &[Id::new(500)],
            &roles,
            &[deny]
        )));
        let allow_member = overwrite(
            300,
            PermissionOverwriteType::Member,
            Permissions::CONNECT,
            Permissions::empty(),
        );
        assert!(can_manage_room(effective_permissions(
            100,
            999,
            300,
            &[Id::new(500)],
            &roles,
            &[deny, allow_member]
        )));
    }

    #[test]
    fn role_allows_aggregate_independent_of_order_and_member_deny_wins() {
        let roles = vec![
            role(100, Permissions::empty()),
            role(500, Permissions::empty()),
            role(501, Permissions::empty()),
        ];
        let deny = overwrite(
            500,
            PermissionOverwriteType::Role,
            Permissions::empty(),
            Permissions::CONNECT,
        );
        let allow = overwrite(
            501,
            PermissionOverwriteType::Role,
            Permissions::CONNECT,
            Permissions::empty(),
        );
        let member_roles = [Id::new(500), Id::new(501)];
        let a =
            effective_permissions(100, 999, 300, &member_roles, &roles, &[deny, allow]).unwrap();
        let b =
            effective_permissions(100, 999, 300, &member_roles, &roles, &[allow, deny]).unwrap();
        assert_eq!(a, b);
        assert!(a.contains(Permissions::CONNECT));
        let member_deny = overwrite(
            300,
            PermissionOverwriteType::Member,
            Permissions::empty(),
            Permissions::CONNECT,
        );
        assert!(!effective_permissions(
            100,
            999,
            300,
            &member_roles,
            &roles,
            &[allow, deny, member_deny]
        )
        .unwrap()
        .contains(Permissions::CONNECT));
    }

    #[test]
    fn incomplete_roles_fail_closed_and_admin_owner_bypass_overwrites() {
        assert_eq!(effective_permissions(100, 999, 300, &[], &[], &[]), None);
        assert!(!can_manage_room(None));
        let roles = vec![
            role(100, Permissions::empty()),
            role(500, Permissions::ADMINISTRATOR),
        ];
        assert_eq!(
            effective_permissions(100, 999, 300, &[Id::new(501)], &roles, &[]),
            None
        );
        let deny = overwrite(
            300,
            PermissionOverwriteType::Member,
            Permissions::empty(),
            Permissions::all(),
        );
        assert!(can_manage_room(effective_permissions(
            100,
            999,
            300,
            &[Id::new(500)],
            &roles,
            &[deny]
        )));
        assert!(can_manage_room(effective_permissions(
            100,
            300,
            300,
            &[],
            &[],
            &[deny]
        )));
    }

    #[test]
    fn vote_kick_enforcement_needs_view_manage_roles_and_move() {
        use twilight_model::guild::Permissions;
        const REQUIRED: Permissions = Permissions::VIEW_CHANNEL
            .union(Permissions::MANAGE_ROLES)
            .union(Permissions::MOVE_MEMBERS);
        // All three bits present allows; extra bits do not deny.
        assert!(can_enforce_kick(Some(REQUIRED)));
        assert!(can_enforce_kick(Some(REQUIRED.union(Permissions::CONNECT))));
        assert!(can_enforce_kick(Some(Permissions::all())));
        // Fail closed on missing snapshots or empty permissions.
        assert!(!can_enforce_kick(None));
        assert!(!can_enforce_kick(Some(Permissions::empty())));
        // Each missing bit denies.
        assert!(!can_enforce_kick(Some(
            Permissions::MANAGE_ROLES.union(Permissions::MOVE_MEMBERS)
        )));
        assert!(!can_enforce_kick(Some(
            Permissions::VIEW_CHANNEL.union(Permissions::MOVE_MEMBERS)
        )));
        assert!(!can_enforce_kick(Some(
            Permissions::VIEW_CHANNEL.union(Permissions::MANAGE_ROLES)
        )));
        // Single bits alone deny.
        assert!(!can_enforce_kick(Some(Permissions::VIEW_CHANNEL)));
        assert!(!can_enforce_kick(Some(Permissions::MANAGE_ROLES)));
        assert!(!can_enforce_kick(Some(Permissions::MOVE_MEMBERS)));
    }

    #[test]
    fn rate_limits_preserve_fractional_retry_after_and_global_scope() {
        assert_eq!(
            classify_response(429, br#"{"retry_after":1.2345,"global":true}"#),
            RoomHttpError::RateLimited {
                retry_after_ms: 1235,
                global: true
            }
        );
        assert_eq!(
            classify_response(429, br#"{"retry_after":-1}"#),
            RoomHttpError::RateLimited {
                retry_after_ms: 1000,
                global: false
            }
        );
        assert_eq!(classify_response(403, b""), RoomHttpError::AccessDenied);
        assert_eq!(classify_response(404, b""), RoomHttpError::NotFound);
        assert_eq!(classify_response(401, b""), RoomHttpError::Unauthorized);
        assert_eq!(classify_response(500, b""), RoomHttpError::UnknownOutcome);
        assert_eq!(
            classify_error_kind(&ErrorType::RequestTimedOut),
            RoomHttpError::UnknownOutcome
        );
        assert_eq!(
            classify_response(400, br#"{"code":50035}"#),
            RoomHttpError::Rejected {
                status: 400,
                code: 50035
            }
        );
    }

    fn companion_plan(overwrites: Vec<ChannelOverwrite>) -> TextChannelPlan {
        TextChannelPlan {
            room_id: 500,
            guild_id: 100,
            name: "voice-chat".to_owned(),
            category_id: 400,
            overwrites,
            settings: two_bot_core::voice_text_channel::TextChannelSettings {
                enabled: true,
                configured_name: None,
                viewer_role_id: None,
            },
        }
    }

    #[test]
    fn companion_view_grant_is_a_member_allow_never_a_deny() {
        let grant = companion_view_grant(300).unwrap();
        assert_eq!(grant.id.get(), 300);
        assert_eq!(grant.kind, PermissionOverwriteType::Member);
        assert_eq!(grant.allow, Permissions::VIEW_CHANNEL);
        assert_eq!(grant.deny, Permissions::empty());
        assert!(companion_view_grant(0).is_none());
    }

    #[test]
    fn companion_overwrites_add_the_bot_view_allow_exactly_once() {
        let plan = companion_plan(vec![ChannelOverwrite {
            target: OverwriteTarget::Everyone,
            allow_view: false,
            deny_view: true,
        }]);
        let overwrites = companion_overwrites(&plan, 999);
        assert_eq!(overwrites.len(), 2);
        assert_eq!(overwrites[0].id.get(), 100);
        assert_eq!(overwrites[0].kind, PermissionOverwriteType::Role);
        assert_eq!(overwrites[0].deny, Permissions::VIEW_CHANNEL);
        assert_eq!(overwrites[1].id.get(), 999);
        assert_eq!(overwrites[1].kind, PermissionOverwriteType::Member);
        assert_eq!(overwrites[1].allow, Permissions::VIEW_CHANNEL);
        assert_eq!(overwrites[1].deny, Permissions::empty());

        // A plan that already grants the bot is not duplicated; a zero bot
        // id adds nothing.
        let planned_bot = companion_plan(vec![ChannelOverwrite {
            target: OverwriteTarget::Member(999),
            allow_view: true,
            deny_view: false,
        }]);
        assert_eq!(companion_overwrites(&planned_bot, 999).len(), 1);
        assert_eq!(companion_overwrites(&plan, 0).len(), 1);
    }

    /// Read one counter series from the process-global metrics exposition.
    fn global_series(prefix: &str) -> u64 {
        two_bot_core::metrics::global()
            .render(None)
            .lines()
            .filter(|line| line.starts_with(prefix))
            .map(|line| {
                line.rsplit_once(' ')
                    .unwrap_or_else(|| panic!("bad sample: {line}"))
                    .1
                    .parse::<u64>()
                    .unwrap_or_else(|_| panic!("bad sample: {line}"))
            })
            .sum()
    }

    #[tokio::test]
    async fn far_future_global_backoff_returns_bounded_ratelimited() {
        // Stall regression (TOG prod stall follow-up): a far-future global
        // `retry-after` must never park the guild actor in a silent sleep.
        // The call returns `RateLimited { global: true }` immediately so the
        // action queue backs off tick-driven, and the cap is counted.
        let http = RoomHttp::new("test-token".to_owned()).unwrap();
        *http.global_not_before.lock().expect("global backoff lock") =
            Some(Instant::now() + Duration::from_secs(3600));
        let capped_before = global_series("two_bot_voice_global_backoff_capped_total");
        let request = Client::new("Bot test-token".to_owned())
            .create_message(Id::<twilight_model::id::marker::ChannelMarker>::new(1))
            .content("ping")
            .try_into_request()
            .expect("request builds offline");
        let started = Instant::now();
        let error = http
            .send_request(request, || true, true)
            .await
            .expect_err("far-future backoff never sends");
        assert!(
            started.elapsed() < Duration::from_secs(GLOBAL_BACKOFF_MAX.as_secs()),
            "returned promptly instead of sleeping out the backoff"
        );
        match error {
            RoomHttpError::RateLimited {
                retry_after_ms,
                global,
            } => {
                assert!(global);
                assert!(retry_after_ms > 0);
            }
            other => panic!("expected global RateLimited, got {other:?}"),
        }
        assert_eq!(
            global_series("two_bot_voice_global_backoff_capped_total"),
            capped_before + 1
        );
        // A stale join still cancels before touching the backoff at all.
        let request = Client::new("Bot test-token".to_owned())
            .create_message(Id::<twilight_model::id::marker::ChannelMarker>::new(1))
            .content("ping")
            .try_into_request()
            .expect("request builds offline");
        let error = http
            .send_request(request, || false, true)
            .await
            .expect_err("stale join cancels");
        assert_eq!(error, RoomHttpError::Cancelled);
    }

    #[test]
    fn republish_rest_age_reports_a_live_stalled_attempt() {
        // The gauge must reflect attempts still live, not just the begin
        // (always 0) and drop (already removed) snapshots: backdate one
        // attempt 7 s and republish mid-flight. The assertion reads the pure
        // computation, not the process-global gauge: the gauge is
        // last-writer-wins and any concurrent test's begin/drop between the
        // publish and the read fails an exact gauge read.
        let slots = Arc::new(Mutex::new(InFlightRest::default()));
        {
            let mut inner = slots.lock().expect("in-flight lock");
            inner.next_id += 1;
            let id = inner.next_id;
            inner.live.insert(
                id,
                Instant::now()
                    .checked_sub(Duration::from_secs(7))
                    .expect("stalled start 7s ago"),
            );
        }
        assert_eq!(oldest_age_secs(&slots), 7);
        publish_rest_age(&slots);
    }
}

#[cfg(test)]
#[path = "voice_rooms_poison_acceptance.rs"]
mod in_flight_poison_acceptance;
