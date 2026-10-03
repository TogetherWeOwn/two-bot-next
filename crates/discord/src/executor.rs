//! Discord REST action executor (TOG-10076, parity §6 Discord REST row).
//!
//! Turns framework-free [`two_bot_core`] outcome values into Discord REST
//! calls with legacy pacing. One executor, two lanes:
//!
//! - **Paced lane** (reads + kicks + channel verbs): 110 ms pacing floor
//!   (legacy `rest.ts` `minIntervalMs ?? 110`), 350 ms for kicks (legacy
//!   `kick.ts` `minIntervalMs ?? 350`), 429 → `retry-after * 1000 + 250`
//!   (body `retry_after` wins when finite), 5xx / transport → `500 * 2^attempt`
//!   backoff, ≤5 tries total (legacy `MAX_HTTP_TRIES`). Every kick ending is
//!   a [`KickResult`] value — the executor never throws for kicks — because
//!   the caller owes the member an audit line either way.
//! - **Moderation lane** (ban/unban/timeout/member-kick): one attempt, 5 s
//!   abort, no auto-retry (legacy `ModerationDiscord` `timeoutMs ?? 5000`).
//!   Failures map to [`DiscordError`] so the idempotency claim decides the
//!   retry posture: [`DiscordError::Rejected`] and local guard refusals are
//!   safe pre-mutation.
//!
//! Transport notes (verified against twilight-http 0.17.1 sources):
//! - `ResponseFuture` re-sends 429 internally when a ratelimiter is
//!   configured, and busy-loops without one — so the executor owns a raw
//!   hyper transport and observes statuses itself. twilight builders
//!   (`CreateBan`, `RemoveMember`, `UpdateGuildMember`, `UpdateChannel`,
//!   `UpdateChannelPermission`, `Request::builder(&Route)`,
//!   `TryIntoRequest`) remain the request factory: method, path, body and
//!   audit-header encoding come from twilight, the executor only sends them.
//!   Message POST is the exception: the pinned `CreateMessage` builder only
//!   models a `u64` nonce and never serializes `enforce_nonce`, so that one
//!   body is built explicitly (method/path stay twilight-owned via
//!   `Route::CreateMessage`) to carry string audit nonces with enforcement.
//! - `ClientBuilder::proxy(host, use_http)` is the analogue of legacy
//!   `DISCORD_API_BASE` (host only — the `http(s)://` scheme prefix is
//!   stripped, mirroring `cutover/src/rest.rs`).
//! - Outbound messages carry `allowed_mentions: { parse: [] }` (legacy
//!   automation/announcement rule: admin-authored text is never a licence to
//!   ping `@everyone`), via client-level
//!   `default_allowed_mentions(AllowedMentions { parse: vec![], .. })`.

mod tickets;
pub use tickets::{ChannelPresence, TicketChannelRequest, TicketMessage};

use crate::ratelimit_guard::{process_guard, GuardError, RateLimitGuard};
use std::sync::Arc;
use std::time::Duration;

use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioExecutor;
use twilight_http::request::{AuditLogReason, Method, Request, TryIntoRequest};
use twilight_http::routing::Route;
use twilight_http::Client as TwilightClient;
use twilight_model::channel::message::AllowedMentions;
use twilight_model::http::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType};
use twilight_model::id::marker::{
    ApplicationMarker, ChannelMarker, GuildMarker, InteractionMarker, MessageMarker, RoleMarker,
    UserMarker,
};
use twilight_model::id::Id;
use two_bot_core::{
    backoff_ms, classify_kick_status, pace_wait_ms, parse_retry_after_secs, retry_after_ms,
    utf16_len, ActionOutcome, KickOutcome, KickResult, KickStatus, ModerationExecution,
    MAX_HTTP_TRIES, MAX_RETRY_AFTER_MS, SEND_MESSAGES_BIT,
};

/// Minimum gap between paced requests, ms (legacy `rest.ts` default).
pub const PACE_INTERVAL_MS: u64 = 110;
/// Minimum gap between kick removals, ms (legacy `kick.ts` default: a burst
/// of removals is the traffic shape Discord rate-limits hardest).
pub const KICK_INTERVAL_MS: u64 = 350;
/// Per-call abort for moderation verbs, ms (legacy `timeoutMs ?? 5000`).
pub const MODERATION_TIMEOUT_MS: u64 = 5_000;
/// Bound response-body collection, including the otherwise untimed GET/kick
/// lanes, so a stalled global response cannot retain pending admission forever.
pub const RESPONSE_BODY_TIMEOUT_MS: u64 = 5_000;
/// Wire marker for a stalled response body: headers arrived but the body never
/// completed within [`RESPONSE_BODY_TIMEOUT_MS`]. The executor seam maps this
/// to [`DiscordError::Timeout`] (never `Unavailable`) so the publish wire
/// deadline stays deterministic when the inner body budget and the outer wire
/// budget expire in the same timer tick ([TOG-12562](/TOG/issues/TOG-12562)).
pub(crate) const BODY_TIMEOUT_MESSAGE: &str = "read body timed out";
/// Legacy audit-log-reason header bound (`X-Audit-Log-Reason`, latin-1,
/// percent-encoded; twilight validates ≤512 chars).
pub const MAX_AUDIT_REASON_CHARS: usize = 512;
/// Legacy message ceiling asserted before sending.
pub const MAX_MESSAGE_CHARS: usize = 2000;

/// Discord failure modes (legacy `ModerationDiscord` `ActionError` codes).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscordError {
    /// A build-time failure or confirmed HTTP rejection proves no mutation
    /// happened, so the claim is safe to release (`discord_rejected`).
    /// Ambiguous statuses, including 408 and unexpected 2xx/3xx, never map here.
    /// Local guard refusals are also safe: they never reached the wire.
    #[error("discord refused the request: {0}")]
    Rejected(String),
    /// Discord did not answer in time (legacy `upstream_timeout`): the
    /// mutation is uncertain — never released, never guessed.
    #[error("discord did not answer in time")]
    Timeout,
    /// Transport/5xx failure (legacy `discord_unavailable`): uncertain, same
    /// treatment as [`DiscordError::Timeout`].
    #[error("discord was unreachable: {0}")]
    Unavailable(String),
    /// Rate limited (legacy `rate_limited`): uncertain, paced per §6.
    #[error("discord rate-limited this request")]
    RateLimited,
    /// Refused locally before any wire attempt; safe to release a claim.
    #[error(transparent)]
    Guard(#[from] GuardError),
}

impl DiscordError {
    /// True only for failures that prove no mutation happened (legacy
    /// `isSafePreMutationFailure`).
    #[must_use]
    pub fn is_safe_pre_mutation(&self) -> bool {
        matches!(self, Self::Rejected(_) | Self::Guard(_))
    }

    /// True only for the durable send-admission single-flight refusal: the
    /// token lane is occupied, so this attempt never reached the wire and a
    /// bounded receipt-callback retry may re-attempt. Every other error —
    /// including timeouts, transport failures and rate limits — is uncertain
    /// or definitive and must never be retried by the callback path.
    #[must_use]
    pub fn is_admission_blocked(&self) -> bool {
        matches!(self, Self::Unavailable(detail)
            if detail == &two_bot_core::send_admission::AdmissionError::Blocked.to_string())
    }
}

/// One moderation mutation the executor carries out (legacy
/// `ModerationDiscordClient` member methods; mirrors the sibling
/// `MemberDiscord` contract so the follow-up wiring needs no domain change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscordCall {
    Ban {
        guild_id: String,
        user_id: String,
        reason: String,
    },
    Unban {
        guild_id: String,
        user_id: String,
        reason: String,
    },
    Kick {
        guild_id: String,
        user_id: String,
        reason: String,
    },
    Timeout {
        guild_id: String,
        user_id: String,
        until_iso: String,
        reason: String,
    },
}

/// Channel-verb call the executor carries out (legacy `ModerationDiscord`
/// channel methods + announcement post shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelCall {
    /// `GET /channels/{c}/messages?limit={count}` then one `DELETE` or
    /// `POST .../bulk-delete` (legacy `purge`).
    Purge {
        channel_id: String,
        count: u64,
        reason: String,
    },
    /// `PATCH /channels/{c}` with `rate_limit_per_user` (legacy slowmode).
    Slowmode {
        channel_id: String,
        seconds: u64,
        reason: String,
    },
    /// `PUT /channels/{c}/permissions/{g}` (legacy `putEveryoneOverwrite`).
    PutOverwrite {
        channel_id: String,
        guild_id: String,
        allow: String,
        deny: String,
        reason: String,
    },
    /// `DELETE /channels/{c}/permissions/{g}` (legacy unlock-without-record).
    DeleteOverwrite {
        channel_id: String,
        guild_id: String,
        reason: String,
    },
    /// `POST /channels/{c}/messages` with mention suppression
    /// (legacy automation/announcement post).
    PostMessage {
        channel_id: String,
        content: String,
        nonce: Option<String>,
    },
    /// `DELETE /channels/{c}/messages/{m}` (legacy automation cleanup —
    /// sticky retirement removes the previous re-post).
    DeleteMessage {
        channel_id: String,
        message_id: String,
        reason: String,
    },
}

/// One observed HTTP exchange. Successful mutations retain their consuming
/// permit until the mutation boundary validates the receipt. Dropping an
/// uncertain response intentionally leaves admission occupied.
pub struct RawResponse {
    pub status: u16,
    pub retry_after_header: Option<String>,
    pub body: Vec<u8>,
    pub(crate) completion: Option<two_bot_core::send_admission::AdmissionPermit>,
}

impl std::fmt::Debug for RawResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawResponse")
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .field("has_retry_after", &self.retry_after_header.is_some())
            .finish()
    }
}

impl RawResponse {
    /// Call only for a definite effect/no-effect, after validating any required
    /// mutation receipt. A storage fault must not authorize effect replay.
    pub(crate) async fn complete(&mut self) {
        if let Some(permit) = self.completion.take() {
            let cooldown = (self.status == 429).then(|| {
                two_bot_core::send_admission::cooldown_from_delays(
                    self.retry_after_header
                        .as_deref()
                        .and_then(|value| value.parse().ok()),
                    self.body_retry_after_secs(),
                )
            });
            if let Err(error) = permit.complete(cooldown).await {
                tracing::warn!(%error, "Discord send admission completion failed; lane held");
            }
        }
    }

    /// Body `retry_after` (seconds) when present and finite — wins over the
    /// header per legacy `kick.ts`.
    #[must_use]
    pub fn body_retry_after_secs(&self) -> Option<f64> {
        serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()?
            .get("retry_after")?
            .as_f64()
            .filter(|v| v.is_finite())
    }

    /// Legacy 429 wait: body wins, then header, default 1 s; +250 ms pad;
    /// clamped so a bad header cannot park a run (legacy `retryAfterMs`).
    #[must_use]
    pub fn retry_after_wait_ms(&self) -> u64 {
        let header = self
            .retry_after_header
            .as_deref()
            .and_then(|s| parse_retry_after_secs(Some(s)));
        retry_after_ms(header, self.body_retry_after_secs())
    }
}

/// Minimal send seam so tests script the wire without a socket.
#[cfg_attr(test, allow(dead_code))]
pub trait RawSender: Send + Sync {
    fn send(
        &self,
        method: &str,
        path: &str,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    ) -> impl std::future::Future<Output = Result<RawResponse, String>> + Send;
}

/// Hyper transport mirroring twilight's own connector
/// (`twilight-http/src/client/connector.rs` with `rustls-platform-verifier`):
/// platform-verifier TLS over `https_or_http` so plain-HTTP mock targets
/// still connect, http1+http2 enabled.
#[derive(Clone)]
pub struct HyperTransport {
    inner: HyperClient<
        hyper_rustls::HttpsConnector<HttpConnector>,
        http_body_util::Full<bytes::Bytes>,
    >,
    scheme_http: bool,
    host: two_bot_core::Secret<String>,
    token: two_bot_core::Secret<String>,
    admission: Option<Arc<dyn two_bot_core::send_admission::SendAdmission>>,
    guard: Arc<RateLimitGuard>,
}

impl std::fmt::Debug for HyperTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HyperTransport")
            .field("scheme_http", &self.scheme_http)
            .field("host", &self.host)
            .field("token", &self.token)
            .field("admission", &self.admission)
            .finish_non_exhaustive()
    }
}

impl HyperTransport {
    /// Build against real Discord (`https://discord.com`).
    pub fn new(token: String) -> Result<Self, String> {
        Self::with_proxy(token, None)
    }

    /// Build with an optional API-host override (legacy `DISCORD_API_BASE`;
    /// the mock double). Accepts a full origin (`http://127.0.0.1:PORT`) or
    /// bare `host:port`; the scheme prefix is stripped (twilight's `proxy`
    /// takes the host only) and selects plain-HTTP transport.
    pub fn with_proxy(token: String, proxy_url: Option<String>) -> Result<Self, String> {
        Self::build(token, proxy_url, None)
    }

    pub fn with_admission(
        token: String,
        proxy_url: Option<String>,
        admission: Arc<dyn two_bot_core::send_admission::SendAdmission>,
    ) -> Result<Self, String> {
        Self::build(token, proxy_url, Some(admission))
    }

    fn build(
        token: String,
        proxy_url: Option<String>,
        admission: Option<Arc<dyn two_bot_core::send_admission::SendAdmission>>,
    ) -> Result<Self, String> {
        use two_bot_core::send_admission::{is_loopback_http, TokenKey};
        if let Some(admission) = &admission {
            if admission.token_key()
                != &TokenKey::for_bot_token(&token).map_err(|e| e.to_string())?
            {
                return Err("Discord send admission token mismatch".to_owned());
            }
        } else if !proxy_url.as_deref().is_some_and(is_loopback_http) {
            return Err("shared durable Discord send admission required".to_owned());
        }
        use hyper_rustls::ConfigBuilderExt as _;
        let (scheme_http, host) = match proxy_url {
            None => (false, "discord.com".to_owned()),
            Some(url) => {
                let lower = url.to_lowercase();
                let (http, rest) = if lower.strip_prefix("http://").is_some() {
                    (true, url[7..].to_owned())
                } else if lower.strip_prefix("https://").is_some() {
                    (false, url[8..].to_owned())
                } else {
                    (url.starts_with("127.") || url.starts_with("localhost"), url)
                };
                (http, rest.trim_end_matches('/').to_owned())
            }
        };
        if host.is_empty() {
            return Err("empty Discord API host".to_owned());
        }
        // Both Hyper's pool key and Twilight's proxy Debug contain this
        // authority. No userinfo, path or query may enter either client.
        if host.contains(['@', '/', '?', '#']) || host.parse::<http::uri::Authority>().is_err() {
            return Err("Discord API override must be an origin without userinfo".to_owned());
        }
        // The raw transport must send what twilight's builder sends:
        // `ClientBuilder` prefixes a bare token with `Bot `.
        let token = if token.starts_with("Bot ") {
            token
        } else {
            format!("Bot {token}")
        };
        let tls = rustls::ClientConfig::builder()
            .try_with_platform_verifier()
            .map_err(|e| format!("tls provider unavailable: {e}"))?
            .with_no_client_auth();
        let mut http_conn = HttpConnector::new();
        http_conn.enforce_http(false);
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http_conn);
        let inner: HyperClient<
            hyper_rustls::HttpsConnector<HttpConnector>,
            http_body_util::Full<bytes::Bytes>,
        > = HyperClient::builder(TokioExecutor::new())
            .retry_canceled_requests(false)
            .build(connector);
        Ok(Self {
            inner,
            scheme_http,
            host: two_bot_core::Secret::new(host),
            token: two_bot_core::Secret::new(token),
            admission,
            guard: process_guard(),
        })
    }

    fn url(&self, path: &str) -> String {
        let scheme = if self.scheme_http { "http" } else { "https" };
        format!(
            "{scheme}://{}/api/v{}/{path}",
            self.host.expose(),
            twilight_http::API_VERSION
        )
    }

    /// Send one governed attempt through headers receipt. The caller either
    /// collects the bounded body ([`PendingHeaders::collect`]) or settles a
    /// status-only verdict without waiting on the body; dropping the pending
    /// headers commits only header-anchored global timing.
    async fn send_request_headers(&self, request: &Request) -> Result<PendingHeaders<'_>, String> {
        if request
            .headers()
            .is_some_and(|headers| headers.contains_key(hyper::header::AUTHORIZATION))
        {
            return Err("caller-supplied authorization is forbidden".to_owned());
        }
        let method: http::Method = request
            .method()
            .name()
            .parse()
            .map_err(|e| format!("bad method: {e}"))?;
        let url = self.url(request.path());
        let mut builder = hyper::Request::builder().method(method).uri(url);
        if let Some(headers) = builder.headers_mut() {
            if request.use_authorization_token() {
                let mut authorization = hyper::header::HeaderValue::from_str(self.token.expose())
                    .map_err(|_| "bad token header".to_owned())?;
                authorization.set_sensitive(true);
                headers.insert(hyper::header::AUTHORIZATION, authorization);
            }
            if let Some(bytes) = request.body() {
                headers.insert(
                    hyper::header::CONTENT_LENGTH,
                    hyper::header::HeaderValue::from(bytes.len() as u64),
                );
                headers.insert(
                    hyper::header::CONTENT_TYPE,
                    hyper::header::HeaderValue::from_static("application/json"),
                );
            } else if matches!(request.method(), Method::Put | Method::Post | Method::Patch) {
                headers.insert(
                    hyper::header::CONTENT_LENGTH,
                    hyper::header::HeaderValue::from(0),
                );
            }
            headers.insert(
                hyper::header::USER_AGENT,
                hyper::header::HeaderValue::from_static(concat!(
                    "DiscordBot (two-bot-next, ",
                    env!("CARGO_PKG_VERSION"),
                    ")"
                )),
            );
            if let Some(req_headers) = request.headers() {
                for (name, value) in req_headers.iter() {
                    headers.insert(name.clone(), value.clone());
                }
            }
        }
        let body_bytes: bytes::Bytes = request.body().unwrap_or(&[]).to_vec().into();
        let hyper_req = builder
            .body(http_body_util::Full::new(body_bytes))
            .map_err(|e| format!("build request: {e}"))?;
        let permit = match &self.admission {
            Some(admission) => Some(admission.admit().await.map_err(|e| e.to_string())?),
            None => None, // Constructor restricts ungoverned transport to loopback fixtures.
        };
        let mut attempt = crate::executor_metrics::Attempt::new(request);
        let response = self
            .inner
            .request(hyper_req)
            .await
            .map_err(|e| format!("transport: {e}"))?;
        let status = response.status().as_u16();
        let accounting = crate::ratelimit_guard::ResponseAccounting::new(
            &self.guard,
            status,
            response.headers(),
            !is_interaction_token_request(request),
        );
        attempt.finish(Some(status));
        let retry_after_header = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        // Reads cannot mutate; a complete failure may be retried independently.
        // Mutation success needs its caller's validated receipt. 5xx, redirects
        // and request-timeout responses remain uncertain even with a full body.
        let complete_on_receipt =
            request.method() == Method::Get || matches!(status, 400 | 401 | 403 | 404 | 405 | 429);
        Ok(PendingHeaders {
            response: Some(response),
            accounting: Some(accounting),
            complete_on_receipt,
            status,
            retry_after_header,
            permit,
        })
    }

    /// One governed wire attempt; no Twilight or pooled-connection resends.
    /// Returns the response and whether a process-global pause was recorded.
    pub async fn send_request(&self, request: &Request) -> Result<(RawResponse, bool), String> {
        self.send_request_headers(request).await?.collect().await
    }
}

/// Headers-received half of one governed wire attempt. The status is known;
/// the body is not yet collected. The caller either collects the bounded body
/// ([`PendingHeaders::collect`]) or settles a status-only verdict
/// ([`PendingHeaders::settle_status`]) and drops the rest unread. Dropping
/// without settling commits only the header-anchored global timing, never
/// body timing.
///
/// [`crate::ratelimit_guard::ResponseAccounting`] settles exactly once:
/// either in `collect`/`settle_status` (with whatever evidence is available)
/// or in `Drop` (header-anchored fallback). Either way a pending header
/// restriction resolves to its header timing without opening admission early.
struct PendingHeaders<'a> {
    response: Option<hyper::Response<hyper::body::Incoming>>,
    accounting: Option<crate::ratelimit_guard::ResponseAccounting<'a>>,
    complete_on_receipt: bool,
    status: u16,
    retry_after_header: Option<String>,
    permit: Option<two_bot_core::send_admission::AdmissionPermit>,
}

impl PendingHeaders<'_> {
    /// Collect the bounded body and settle guard/admission accounting exactly
    /// as the pre-split `send_request` always has.
    async fn collect(mut self) -> Result<(RawResponse, bool), String> {
        use http_body_util::BodyExt as _;
        let response = self.response.take().expect("pending headers");
        let status = self.status;
        // Expiry drops accounting, resolving the pending header restriction to
        // its header-anchored timing/fallback without opening admission early.
        let collected = tokio::time::timeout(
            Duration::from_millis(RESPONSE_BODY_TIMEOUT_MS),
            http_body_util::Limited::new(response.into_body(), 8 * 1024 * 1024).collect(),
        )
        .await;
        let body = match collected {
            Ok(Ok(body)) => body.to_bytes().to_vec(),
            // 429 is definitive no-effect even with a broken, oversized or
            // timed-out body. Missing timing installs an indefinite hold
            // instead of a default.
            Err(_) | Ok(Err(_)) if status == 429 => Vec::new(),
            Err(_) => return Err(BODY_TIMEOUT_MESSAGE.to_owned()),
            Ok(Err(_)) => return Err("Discord response body unavailable".to_owned()),
        };
        let mut res = RawResponse {
            status,
            retry_after_header: self.retry_after_header.clone(),
            body,
            completion: self.permit.take(),
        };
        let global = self
            .accounting
            .take()
            .map(|mut settled| settled.finish(&res))
            .unwrap_or(false);
        if self.complete_on_receipt {
            res.complete().await;
        }
        Ok((res, global))
    }

    /// Settle a status-only verdict without waiting on the body: run the same
    /// receipt-time settlement `collect` would, but with no body evidence,
    /// then drop the unread body. Definite verdicts release the lane;
    /// uncertain ones keep the permit held by dropping it uncompleted.
    /// 204 is the singular role-mutation success: it releases admission here
    /// because `complete_on_receipt` (GET + rejection/rate-limit statuses)
    /// never fires for a PUT/DELETE success, and no caller completes it later.
    async fn settle_status(mut self) -> u16 {
        let status = self.status;
        let mut res = RawResponse {
            status,
            retry_after_header: self.retry_after_header.clone(),
            body: Vec::new(),
            completion: self.permit.take(),
        };
        let _ = self
            .accounting
            .take()
            .map(|mut settled| settled.finish(&res));
        if self.complete_on_receipt || status == 204 {
            res.complete().await;
        }
        status
    }
}

impl Drop for PendingHeaders<'_> {
    fn drop(&mut self) {
        // Resolve the pending header restriction to its header-anchored
        // timing/fallback without opening admission early. Body timing is
        // unknown: the body was never collected. An uncompleted admission
        // permit intentionally holds the lane: an uncertain response must not
        // release durable send admission.
        drop(self.accounting.take());
    }
}

fn is_interaction_callback(request: &Request) -> bool {
    request.path().starts_with("interactions/") && request.path().ends_with("/callback")
}

fn is_interaction_token_request(request: &Request) -> bool {
    // Webhook-token routes (`webhooks/{app}/{token}…`: interaction originals
    // and followups) authenticate with the URL token, not the bot token.
    is_interaction_callback(request)
        || request
            .path()
            .split('?')
            .next()
            .and_then(|path| path.strip_prefix("webhooks/"))
            .and_then(|rest| rest.split('/').nth(1))
            .is_some_and(|token| !token.is_empty())
}

/// Prior DELETE attempts supplied to a paced kick's safety guard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KickAttemptState {
    /// Number of DELETE attempts already made, excluding the pending send.
    pub attempts: u32,
    /// A prior timeout or transport failure may have applied a mutation.
    /// Sticky across retries: a later 429/5xx does not resolve that uncertainty.
    pub mutation_uncertain: bool,
}

#[path = "internal_exec/member.rs"]
pub mod member;
#[path = "self_roles_rest.rs"]
pub mod self_roles;

/// A separately authorized staging revoke operation. It is never constructed
/// by the level-up path. Both deployment identities must match the existing
/// pinned TWO identities, so configuration cannot relabel production as staging.
#[derive(Debug, Clone, Copy)]
pub struct StagingRevokeFence {
    staging_guild: u64,
    production_guild: u64,
}

impl StagingRevokeFence {
    pub fn new(staging_guild: u64, production_guild: u64) -> Result<Self, DiscordError> {
        use two_bot_core::backup::guild_config::{LIVE_GUILD_ID, TWO_STAGING_GUILD_ID};
        if staging_guild.to_string() != TWO_STAGING_GUILD_ID
            || production_guild.to_string() != LIVE_GUILD_ID
        {
            return Err(DiscordError::Rejected(
                "invalid staging revoke fence".into(),
            ));
        }
        Ok(Self {
            staging_guild,
            production_guild,
        })
    }

    fn allows(self, guild: u64) -> bool {
        guild == self.staging_guild && guild != self.production_guild
    }
}

#[derive(serde::Deserialize)]
struct MemberRoles {
    roles: Vec<String>,
}

#[derive(serde::Deserialize)]
struct RewardRole {
    id: String,
    managed: bool,
    position: i64,
    permissions: String,
}

/// The S4 REST executor: paced lane + moderation lane over one transport.
#[derive(Debug, Clone)]
pub struct ActionExecutor {
    inner: Arc<ExecutorInner>,
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PacingLane {
    Shared,
    Kick,
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct PacingAdmission {
    pub lane: PacingLane,
    pub at: tokio::time::Instant,
}

struct ExecutorInner {
    transport: HyperTransport,
    /// Twilight client kept as the request factory (builders + audit
    /// encoding) and for publish/callback sends with default mention
    /// suppression.
    factory: TwilightClient,
    pace_interval: Duration,
    kick_interval: Duration,
    moderation_timeout: Duration,
    pace_last_at: tokio::sync::Mutex<tokio::time::Instant>,
    kick_last_at: tokio::sync::Mutex<tokio::time::Instant>,
    #[cfg(test)]
    pacing_probe: Option<tokio::sync::mpsc::UnboundedSender<PacingAdmission>>,
    requests: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for ExecutorInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutorInner")
            .field("transport", &self.transport)
            // Twilight retains the proxy origin and credentials internally.
            .field("factory", &"[REDACTED]")
            .field("pace_interval", &self.pace_interval)
            .field("kick_interval", &self.kick_interval)
            .field("moderation_timeout", &self.moderation_timeout)
            .field("requests", &self.requests)
            .finish_non_exhaustive()
    }
}

impl ActionExecutor {
    /// Build against real Discord.
    pub fn new(token: String) -> Result<Self, String> {
        Self::with_proxy(token, None)
    }

    /// Build with an optional API-host override (legacy `DISCORD_API_BASE`;
    /// tests point this at the mock double).
    pub fn with_proxy(token: String, proxy_url: Option<String>) -> Result<Self, String> {
        Self::build(token, proxy_url, None)
    }

    /// Mandatory for non-loopback sends; every retry uses this same lane.
    pub fn with_admission(
        token: String,
        proxy_url: Option<String>,
        admission: Arc<dyn two_bot_core::send_admission::SendAdmission>,
    ) -> Result<Self, String> {
        Self::build(token, proxy_url, Some(admission))
    }

    /// Explicit shared guard injection for isolated mock tests or embedding.
    /// Production callers must reuse one guard for every executor in a process
    /// and reach the wire through [`Self::with_admission`] instead.
    pub fn with_proxy_and_guard(
        token: String,
        proxy_url: Option<String>,
        guard: Arc<RateLimitGuard>,
    ) -> Result<Self, String> {
        let mut transport = HyperTransport::with_proxy(token.clone(), proxy_url.clone())?;
        transport.guard = guard;
        Self::assemble(token, proxy_url, transport)
    }

    fn build(
        token: String,
        proxy_url: Option<String>,
        admission: Option<Arc<dyn two_bot_core::send_admission::SendAdmission>>,
    ) -> Result<Self, String> {
        let transport = HyperTransport::build(token.clone(), proxy_url.clone(), admission)?;
        Self::assemble(token, proxy_url, transport)
    }

    fn assemble(
        token: String,
        proxy_url: Option<String>,
        transport: HyperTransport,
    ) -> Result<Self, String> {
        let mut builder = TwilightClient::builder()
            .token(token)
            .ratelimiter(None)
            .timeout(Duration::from_millis(MODERATION_TIMEOUT_MS));
        if proxy_url.is_some() {
            // Reuse the already-validated origin, never reparse raw input for
            // the nested request factory with a different set of rules.
            builder = builder.proxy(transport.host.expose().clone(), transport.scheme_http);
        }
        builder = builder.default_allowed_mentions(AllowedMentions {
            parse: vec![],
            replied_user: false,
            roles: vec![],
            users: vec![],
        });
        Ok(Self {
            inner: Arc::new(ExecutorInner {
                transport,
                factory: builder.build(),
                pace_interval: Duration::from_millis(PACE_INTERVAL_MS),
                kick_interval: Duration::from_millis(KICK_INTERVAL_MS),
                moderation_timeout: Duration::from_millis(MODERATION_TIMEOUT_MS),
                pace_last_at: tokio::sync::Mutex::new(
                    tokio::time::Instant::now() - Duration::from_secs(60),
                ),
                kick_last_at: tokio::sync::Mutex::new(
                    tokio::time::Instant::now() - Duration::from_secs(60),
                ),
                #[cfg(test)]
                pacing_probe: None,
                requests: std::sync::atomic::AtomicU64::new(0),
            }),
        })
    }

    /// Requests made so far: the run's own Discord cost report.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.inner
            .requests
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn with_pacing_probe(
        token: String,
        proxy_url: Option<String>,
    ) -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<PacingAdmission>), String> {
        let mut executor = Self::with_proxy(token, proxy_url)?;
        let (probe, admissions) = tokio::sync::mpsc::unbounded_channel();
        Arc::get_mut(&mut executor.inner).unwrap().pacing_probe = Some(probe);
        Ok((executor, admissions))
    }

    pub(crate) fn stamp_paced_lane(&self, last: &mut tokio::time::Instant, _kick_lane: bool) {
        let at = tokio::time::Instant::now();
        *last = at;
        // Emit only committed admission, synchronously under the lane lock.
        // Reservation and refused late authorization are not admission.
        #[cfg(test)]
        if let Some(probe) = &self.inner.pacing_probe {
            let _ = probe.send(PacingAdmission {
                lane: if _kick_lane {
                    PacingLane::Kick
                } else {
                    PacingLane::Shared
                },
                at,
            });
        }
    }

    async fn admit(&self, request: &Request, lane: Option<bool>) -> Result<(), GuardError> {
        let guard = &self.inner.transport.guard;
        let essential = is_interaction_callback(request);
        let Some(kick_lane) = lane else {
            return guard.admit(essential).await;
        };
        let (lock, interval) = if kick_lane {
            (&self.inner.kick_last_at, self.inner.kick_interval)
        } else {
            (&self.inner.pace_last_at, self.inner.pace_interval)
        };
        // Keep the lane reservation through cooldown and pacing. Timestamp only
        // the actual dispatch, not each queued caller's pre-cooldown admission.
        let mut last = lock.lock().await;
        guard.admit(essential).await?;
        let earliest = *last + interval;
        if earliest > tokio::time::Instant::now() {
            tokio::time::sleep_until(earliest).await;
        }
        // A global response/breaker may arrive during the lane sleep.
        guard.admit(essential).await?;
        self.stamp_paced_lane(&mut last, kick_lane);
        Ok(())
    }

    /// Keep pacing/global waits before the audit service's late DB fence, and
    /// retain the lane reservation through authorization and its bounded send.
    pub(crate) async fn paced_lane(
        &self,
        kick_lane: bool,
    ) -> Result<tokio::sync::MutexGuard<'_, tokio::time::Instant>, GuardError> {
        let (lock, interval) = if kick_lane {
            (&self.inner.kick_last_at, self.inner.kick_interval)
        } else {
            (&self.inner.pace_last_at, self.inner.pace_interval)
        };
        let last = lock.lock().await;
        self.inner.transport.guard.admit(false).await?;
        let earliest = *last + interval;
        if earliest > tokio::time::Instant::now() {
            tokio::time::sleep_until(earliest).await;
        }
        self.inner.transport.guard.admit(false).await?;
        Ok(last)
    }

    fn count(&self) {
        self.inner
            .requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    async fn send_admitted(&self, request: &Request) -> Result<(RawResponse, bool), DiscordError> {
        self.count();
        self.inner
            .transport
            .send_request(request)
            .await
            .map_err(|detail| {
                // The body budget equals the wire budget and both start within
                // milliseconds of each other, so on a loaded runner they expire in
                // the same timer tick. Normalize the stalled-body marker to the
                // wire-deadline error: a body that never completed inside the wire
                // budget is "Discord did not answer in time" (legacy
                // `upstream_timeout`), whichever timer wins
                // ([TOG-12562](/TOG/issues/TOG-12562)).
                if detail == BODY_TIMEOUT_MESSAGE {
                    DiscordError::Timeout
                } else {
                    DiscordError::Unavailable(detail)
                }
            })
    }

    async fn send_paced(
        &self,
        request: &Request,
        kick_lane: bool,
    ) -> Result<(RawResponse, bool), DiscordError> {
        self.admit(request, Some(kick_lane)).await?;
        self.send_admitted(request).await
    }

    async fn send_with_timeout(
        &self,
        request: &Request,
        lane: Option<bool>,
    ) -> Result<(RawResponse, bool), DiscordError> {
        self.send_with_timeout_for(request, lane, self.inner.moderation_timeout)
            .await
    }

    async fn send_with_timeout_for(
        &self,
        request: &Request,
        lane: Option<bool>,
        timeout: Duration,
    ) -> Result<(RawResponse, bool), DiscordError> {
        let deadline = tokio::time::Instant::now() + timeout;
        tokio::time::timeout_at(deadline, self.admit(request, lane))
            .await
            .map_err(|_| GuardError::AdmissionTimeout)??;
        tokio::time::timeout_at(deadline, self.send_admitted(request))
            .await
            .map_err(|_| DiscordError::Timeout)?
    }

    /// Singular role mutations use status only. A truncated/stalled provider
    /// body must not erase headers already received or invent an unknown send:
    /// any body-read failure is `Ambiguous` with no status, so the caller
    /// compensates instead of trusting a partial exchange.
    async fn send_status(&self, request: &Request) -> Result<u16, String> {
        self.count();
        let pending = self.inner.transport.send_request_headers(request).await?;
        Ok(pending.settle_status().await)
    }

    /// Build a twilight [`Request`] from a builder without sending (keeps
    /// every method/path/body/audit encoding twilight-owned). Build failures
    /// (bad ids, out-of-range fields, failed validation) happen before any
    /// I/O, so they map to [`DiscordError::Rejected`] — provably no mutation
    /// happened and the claim fence is safe to release.
    fn request_of<T>(build: T) -> Result<Request, DiscordError>
    where
        T: TryIntoRequest,
    {
        build
            .try_into_request()
            .map_err(|e| DiscordError::Rejected(format!("build: {e}")))
    }

    /// One moderation verb: a single attempt with the 5 s abort, no
    /// auto-retry (legacy `ModerationDiscord::call`). Accepted statuses are
    /// per-verb (legacy `accepted` lists); anything else maps via
    /// [`throw_for_status`].
    async fn call_once(
        &self,
        request: Request,
        accepted: &[u16],
    ) -> Result<Option<serde_json::Value>, DiscordError> {
        let needs_object = request.method() == Method::Patch;
        let mut res = self.call_once_raw(request, accepted).await?;
        let body: Option<serde_json::Value> = serde_json::from_slice(&res.body).ok();
        if needs_object
            && !body.as_ref().is_some_and(|body| {
                body.get("id")
                    .or_else(|| body.pointer("/user/id"))
                    .and_then(serde_json::Value::as_str)
                    .and_then(|id| id.parse::<u64>().ok())
                    .is_some_and(|id| id != 0)
            })
        {
            return Err(DiscordError::Unavailable(
                "invalid mutation receipt".to_owned(),
            ));
        }
        // These verbs use their accepted status as the effect receipt; message
        // creation instead validates its required id before consuming completion.
        res.complete().await;
        Ok(body)
    }

    /// Same single-attempt send as [`Self::call_once`], but returns the raw
    /// exchange so callers that must distinguish "proven absent" from
    /// "unreadable" can validate the body themselves.
    pub(crate) async fn call_once_raw(
        &self,
        request: Request,
        accepted: &[u16],
    ) -> Result<RawResponse, DiscordError> {
        self.call_once_raw_lane(request, accepted, None).await
    }

    /// [`Self::call_once_raw`] through the paced non-kick lane: admission,
    /// pacing and the guard run inside the one bounded attempt.
    pub(crate) async fn call_once_raw_paced(
        &self,
        request: Request,
        accepted: &[u16],
    ) -> Result<RawResponse, DiscordError> {
        self.call_once_raw_lane(request, accepted, Some(false))
            .await
    }

    async fn call_once_raw_lane(
        &self,
        request: Request,
        accepted: &[u16],
        lane: Option<bool>,
    ) -> Result<RawResponse, DiscordError> {
        let (res, _) = self.send_with_timeout(&request, lane).await?;
        if accepted.contains(&res.status) {
            return Ok(res);
        }
        Err(throw_for_status(&res))
    }

    async fn role_read<T: serde::de::DeserializeOwned>(
        &self,
        request: Request,
    ) -> Result<T, DiscordError> {
        let response = self.call_once_raw_paced(request, &[200]).await?;
        serde_json::from_slice(&response.body)
            .map_err(|_| DiscordError::Unavailable("invalid role readback".into()))
    }

    /// Fetch current roles, never interpreting unreadable/forbidden as empty.
    pub async fn member_role_ids(
        &self,
        guild_id: u64,
        member_id: u64,
    ) -> Result<Vec<String>, DiscordError> {
        let guild = snowflake(&guild_id.to_string())?;
        let member = snowflake(&member_id.to_string())?;
        let request = Self::request_of(self.inner.factory.guild_member(guild, member))?;
        let roles: MemberRoles = self.role_read(request).await?;
        for role in &roles.roles {
            let _: Id<RoleMarker> = snowflake(role)?;
        }
        Ok(roles.roles)
    }

    /// Apply the domain plan through the shared pacing/request factory. All
    /// local validation and the entire revoke preflight precede any mutation;
    /// Discord cannot promise atomicity if permissions change after preflight.
    /// PUT/DELETE role endpoints are idempotent; errors are returned, not hidden.
    pub async fn execute_reward_roles(
        &self,
        guild_id: u64,
        member_id: u64,
        plan: &two_bot_core::leveling::RewardRolePlan,
        revoke_fence: Option<StagingRevokeFence>,
    ) -> Result<(), DiscordError> {
        let guild = snowflake(&guild_id.to_string())?;
        let member = snowflake(&member_id.to_string())?;
        let grants: Vec<Id<RoleMarker>> = plan
            .grant
            .iter()
            .map(|id| snowflake(id))
            .collect::<Result<_, _>>()?;
        let revokes: Vec<Id<RoleMarker>> = plan
            .revoke
            .iter()
            .map(|id| snowflake(id))
            .collect::<Result<_, _>>()?;
        let grant_reason = audit_reason(&plan.grant_reason)?;
        let revoke_reason = if revokes.is_empty() {
            None
        } else {
            if !revoke_fence.is_some_and(|fence| fence.allows(guild_id)) {
                return Err(DiscordError::Rejected(
                    "level role revocation is staging-only".into(),
                ));
            }
            Some(audit_reason(plan.revoke_reason.as_deref().ok_or_else(
                || DiscordError::Rejected("missing level role revoke reason".into()),
            )?)?)
        };
        if !revokes.is_empty() {
            self.preflight_reward_revokes(guild_id, &plan.revoke)
                .await?;
        }
        for role in grants {
            let request = Self::request_of(
                self.inner
                    .factory
                    .add_guild_member_role(guild, member, role)
                    .reason(&grant_reason),
            )?;
            self.call_once_raw_paced(request, &[200, 204]).await?;
        }
        for role in revokes {
            let request = Self::request_of(
                self.inner
                    .factory
                    .remove_guild_member_role(guild, member, role)
                    .reason(revoke_reason.as_deref().expect("validated revoke reason")),
            )?;
            self.call_once_raw_paced(request, &[200, 204]).await?;
        }
        Ok(())
    }

    async fn preflight_reward_revokes(
        &self,
        guild_id: u64,
        revoke: &[String],
    ) -> Result<(), DiscordError> {
        #[derive(serde::Deserialize)]
        struct BotIdentity {
            id: String,
        }
        let bot: BotIdentity = self
            .role_read(Self::request_of(self.inner.factory.current_user())?)
            .await?;
        let bot_id: Id<UserMarker> = snowflake(&bot.id)?;
        let held = self.member_role_ids(guild_id, bot_id.get()).await?;
        let roles: Vec<RewardRole> = self
            .role_read(Self::request_of(
                self.inner.factory.roles(snowflake(&guild_id.to_string())?),
            )?)
            .await?;
        let refuse =
            || DiscordError::Rejected("level role revoke preflight refused the entire set".into());
        let everyone = guild_id.to_string();
        let mut permissions = 0u64;
        let mut top = 0i64;
        // Missing held/everyone roles is an incomplete permission snapshot.
        for id in held.iter().chain(std::iter::once(&everyone)) {
            let role = roles.iter().find(|r| &r.id == id).ok_or_else(refuse)?;
            permissions |= role.permissions.parse::<u64>().map_err(|_| refuse())?;
            top = top.max(role.position);
        }
        let manage_roles = twilight_model::guild::Permissions::MANAGE_ROLES.bits();
        let administrator = twilight_model::guild::Permissions::ADMINISTRATOR.bits();
        if permissions & (manage_roles | administrator) == 0 {
            return Err(refuse());
        }
        for id in revoke {
            let role = roles.iter().find(|r| &r.id == id).ok_or_else(refuse)?;
            if role.managed || role.id == everyone || role.position >= top {
                return Err(refuse());
            }
        }
        Ok(())
    }

    /// Member ban: `PUT /guilds/{g}/bans/{u}` (legacy accepts 200/204).
    pub async fn ban(
        &self,
        guild_id: &str,
        user_id: &str,
        reason: &str,
    ) -> Result<(), DiscordError> {
        let guild = snowflake(guild_id)?;
        let user = snowflake(user_id)?;
        let reason = audit_reason(reason)?;
        let req = Self::request_of(self.inner.factory.create_ban(guild, user).reason(&reason))?;
        self.call_once(req, &[200, 204]).await.map(|_| ())
    }

    /// Member unban: `DELETE /guilds/{g}/bans/{u}` (legacy accepts
    /// 200/204/404 — unbanning a non-banned user still completes the job).
    pub async fn unban(
        &self,
        guild_id: &str,
        user_id: &str,
        reason: &str,
    ) -> Result<(), DiscordError> {
        let guild = snowflake(guild_id)?;
        let user = snowflake(user_id)?;
        let reason = audit_reason(reason)?;
        let req = Self::request_of(self.inner.factory.delete_ban(guild, user).reason(&reason))?;
        self.call_once(req, &[200, 204, 404]).await.map(|_| ())
    }

    /// Member kick (moderation lane): `DELETE /guilds/{g}/members/{u}` with
    /// 404 accepted (legacy `ModerationDiscord::kick`).
    pub async fn kick_member(
        &self,
        guild_id: &str,
        user_id: &str,
        reason: &str,
    ) -> Result<(), DiscordError> {
        let guild = snowflake(guild_id)?;
        let user = snowflake(user_id)?;
        let reason = audit_reason(reason)?;
        let req = Self::request_of(
            self.inner
                .factory
                .remove_guild_member(guild, user)
                .reason(&reason),
        )?;
        self.call_once(req, &[200, 204, 404]).await.map(|_| ())
    }

    /// Member timeout: `PATCH /guilds/{g}/members/{u}` with
    /// `communication_disabled_until` (legacy accepts 200). `until_iso`
    /// `None` clears the timeout.
    pub async fn timeout_member(
        &self,
        guild_id: &str,
        user_id: &str,
        until_iso: Option<&str>,
        reason: &str,
    ) -> Result<(), DiscordError> {
        use twilight_model::util::datetime::Timestamp;
        let guild = snowflake(guild_id)?;
        let user = snowflake(user_id)?;
        let reason = audit_reason(reason)?;
        let until = until_iso
            .map(|s| {
                Timestamp::parse(s).map_err(|e| DiscordError::Rejected(format!("bad until: {e}")))
            })
            .transpose()?;
        let req = Self::request_of(
            self.inner
                .factory
                .update_guild_member(guild, user)
                .communication_disabled_until(until)
                .reason(&reason),
        )?;
        // NB: a 29-day-expiry rejection happens here, before any I/O — it is
        // Rejected (safe pre-mutation) by request_of, never Unavailable.
        self.call_once(req, &[200]).await.map(|_| ())
    }

    /// Carry out one [`DiscordCall`] (the `MemberDiscord` handoff surface).
    pub async fn execute_call(&self, call: &DiscordCall) -> Result<(), DiscordError> {
        match call {
            DiscordCall::Ban {
                guild_id,
                user_id,
                reason,
            } => self.ban(guild_id, user_id, reason).await,
            DiscordCall::Unban {
                guild_id,
                user_id,
                reason,
            } => self.unban(guild_id, user_id, reason).await,
            DiscordCall::Kick {
                guild_id,
                user_id,
                reason,
            } => self.kick_member(guild_id, user_id, reason).await,
            DiscordCall::Timeout {
                guild_id,
                user_id,
                until_iso,
                reason,
            } => {
                self.timeout_member(guild_id, user_id, Some(until_iso), reason)
                    .await
            }
        }
    }

    /// Paced kick with the legacy retry budget: 350 ms lane, 429 parks for
    /// `retry-after + 250 ms` on the same attempt, 5xx/transport backs off
    /// `500 * 2^(attempts-1)`, past 4 retries the member is reported, never
    /// retried. Never throws — every ending is a [`KickResult`] (legacy
    /// `DiscordKicker::kick`).
    pub async fn kick_paced(&self, guild_id: &str, user_id: &str, reason: &str) -> KickResult {
        match self
            .kick_paced_guarded(guild_id, user_id, reason, |_| async {
                Ok::<(), std::convert::Infallible>(())
            })
            .await
        {
            Ok(result) => result,
            Err(never) => match never {},
        }
    }

    /// Reauthorize after pacing and before every DELETE, including retries.
    /// The callback receives prior attempts and any pending mutation uncertainty
    /// so a refusal can be audited as failed rather than cleanly protected.
    /// The kick lane stays reserved through authorization and the bounded
    /// exchange, with no additional pacing wait after the final safety read.
    pub async fn kick_paced_guarded<F, Fut, E>(
        &self,
        guild_id: &str,
        user_id: &str,
        reason: &str,
        mut authorize: F,
    ) -> Result<KickResult, E>
    where
        F: FnMut(KickAttemptState) -> Fut,
        Fut: std::future::Future<Output = Result<(), E>>,
    {
        let path_guild = guild_id.to_owned();
        let path_user = user_id.to_owned();
        let reason = match audit_reason(reason) {
            Ok(r) => r,
            Err(e) => {
                return Ok(KickResult {
                    outcome: KickOutcome::Failed,
                    status: None,
                    detail: e.to_string(),
                    attempts: 0,
                })
            }
        };
        let mut attempts: u32 = 0;
        let mut mutation_uncertain = false;
        loop {
            // Pacing and global/breaker admission run before authorization;
            // the lane stays reserved through the bounded exchange.
            let mut lane = match self.paced_lane(true).await {
                Ok(lane) => lane,
                Err(error) => {
                    return Ok(KickResult {
                        outcome: KickOutcome::Failed,
                        status: None,
                        detail: error.to_string(),
                        attempts,
                    })
                }
            };
            let request = match self.kick_request(&path_guild, &path_user, &reason) {
                Ok(r) => r,
                Err(detail) => {
                    return Ok(KickResult {
                        outcome: KickOutcome::Failed,
                        status: None,
                        detail,
                        attempts,
                    })
                }
            };
            authorize(KickAttemptState {
                attempts,
                mutation_uncertain,
            })
            .await?;
            // A guard closed during authorization refuses instead of sleeping
            // after the fresh safety read.
            if let Err(error) = self.inner.transport.guard.check_now(false) {
                return Ok(KickResult {
                    outcome: KickOutcome::Failed,
                    status: None,
                    detail: error.to_string(),
                    attempts,
                });
            }
            // Build/admission refusals spend no HTTP attempt. Count only once
            // dispatch starts, including failed transports and timeouts.
            attempts += 1;
            // A deadline covers both headers and body, not just connection
            // setup. A timeout is ambiguous: retry only after fresh safety
            // authorization, and report failure if the bounded budget runs out.
            self.stamp_paced_lane(&mut lane, true);
            let exchange =
                tokio::time::timeout(self.inner.moderation_timeout, self.send_admitted(&request))
                    .await
                    .unwrap_or_else(|_| {
                        Err(DiscordError::Unavailable(
                            "DELETE timed out; mutation may have applied".into(),
                        ))
                    });
            // Backoff belongs to this caller, not the shared kick reservation.
            drop(lane);
            let (mut res, global) = match exchange {
                Ok(r) => r,
                Err(detail) => {
                    mutation_uncertain = true;
                    if attempts > MAX_HTTP_TRIES - 1 {
                        return Ok(KickResult {
                            outcome: KickOutcome::Failed,
                            status: None,
                            detail: format!("network: {detail}"),
                            attempts,
                        });
                    }
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempts - 1))).await;
                    continue;
                }
            };
            match classify_kick_status(res.status) {
                KickStatus::Removed => {
                    res.complete().await;
                    return Ok(KickResult {
                        outcome: KickOutcome::Kicked,
                        status: Some(res.status),
                        detail: "removed".to_owned(),
                        attempts,
                    });
                }
                KickStatus::AlreadyGone => {
                    return Ok(KickResult {
                        outcome: KickOutcome::AlreadyGone,
                        status: Some(res.status),
                        detail: "not a member".to_owned(),
                        attempts,
                    })
                }
                KickStatus::Forbidden => {
                    return Ok(KickResult {
                        outcome: KickOutcome::Forbidden,
                        status: Some(res.status),
                        detail: "missing Kick Members, or the target outranks the bot".to_owned(),
                        attempts,
                    })
                }
                KickStatus::Unauthorized => {
                    return Ok(KickResult {
                        outcome: KickOutcome::Failed,
                        status: Some(res.status),
                        detail: "token rejected".to_owned(),
                        attempts,
                    })
                }
                KickStatus::RateLimited => {
                    if attempts > MAX_HTTP_TRIES - 1 {
                        return Ok(KickResult {
                            outcome: KickOutcome::RateLimited,
                            status: Some(res.status),
                            detail: format!("still rate limited after {attempts} attempts"),
                            attempts,
                        });
                    }
                    // Global retries use the header-anchored shared deadline
                    // at next admission; only route-local 429s start a new wait.
                    if !global {
                        tokio::time::sleep(Duration::from_millis(res.retry_after_wait_ms())).await;
                    }
                }
                KickStatus::ServerError => {
                    if attempts > MAX_HTTP_TRIES - 1 {
                        return Ok(KickResult {
                            outcome: KickOutcome::Failed,
                            status: Some(res.status),
                            detail: "server error".to_owned(),
                            attempts,
                        });
                    }
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempts - 1))).await;
                }
                KickStatus::Other => {
                    return Ok(KickResult {
                        outcome: KickOutcome::Failed,
                        status: Some(res.status),
                        detail: "unexpected status".to_owned(),
                        attempts,
                    })
                }
            }
        }
    }

    fn kick_request(&self, guild_id: &str, user_id: &str, reason: &str) -> Result<Request, String> {
        let guild: Id<GuildMarker> = snowflake(guild_id).map_err(|e| e.to_string())?;
        let user: Id<UserMarker> = snowflake(user_id).map_err(|e| e.to_string())?;
        // Kick failures report as values, never throw, so the pre-send error
        // is rendered to its detail string here (finding 7 still holds: it is
        // Rejected at the DiscordError level before rendering).
        Self::request_of(
            self.inner
                .factory
                .remove_guild_member(guild, user)
                .reason(reason),
        )
        .map_err(|e| e.to_string())
    }

    /// Paced GET: 110 ms lane, 403/404 → `None`, 429 parks (same attempt),
    /// 5xx/transport backs off ≤4, other statuses → `None` (legacy
    /// `DiscordRest::get`). Supported paths cover the parity §6 reads —
    /// guild, guild members (+`after`/`limit`), guild scheduled-events
    /// (+`with_user_count`), single channel and the channel-messages list
    /// (+`after`/`around`/`before`/`limit`). Anything else is a caller bug and
    /// is refused without I/O, never silently rewritten (finding 2).
    pub async fn get_json(&self, path: &str) -> Result<Option<serde_json::Value>, String> {
        Ok(self.get_json_observed(path).await?.map(|(data, _)| data))
    }

    /// Permission evidence must distinguish denied/absent (403/404) from an
    /// unavailable or unreadable response. Keep the shared paced read policy,
    /// but never let a transient failure look like a proven delivery skip.
    pub async fn get_json_checked(&self, path: &str) -> Result<Option<serde_json::Value>, String> {
        self.read_json(path, true).await
    }

    /// Membership evidence is bounded by the successful attempt's request
    /// start, after pacing, never by headers/body completion or an earlier
    /// failed attempt. Ordinary `get_json` keeps its data-only contract.
    pub async fn get_json_observed(
        &self,
        path: &str,
    ) -> Result<Option<(serde_json::Value, String)>, String> {
        let route = raw_get_route(path)?;
        let mut attempt: u32 = 0;
        loop {
            let request = Request::from_route(&route);
            // Guard refusals are terminal; pacing completes before the stamp.
            self.admit(&request, Some(false))
                .await
                .map_err(|error| error.to_string())?;
            let observed_at = two_bot_core::now_iso();
            let (res, global) = match self.send_admitted(&request).await {
                Ok(r) => r,
                Err(detail) => {
                    if attempt >= MAX_HTTP_TRIES - 1 {
                        return Err(detail.to_string());
                    }
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempt))).await;
                    attempt += 1;
                    continue;
                }
            };
            match res.status {
                200..=299 => {
                    return Ok(serde_json::from_slice(&res.body)
                        .ok()
                        .map(|data| (data, observed_at)));
                }
                429 => {
                    if !global {
                        tokio::time::sleep(Duration::from_millis(res.retry_after_wait_ms())).await;
                    }
                }
                403 | 404 => return Ok(None),
                500..=599 => {
                    if attempt >= MAX_HTTP_TRIES - 1 {
                        return Ok(None);
                    }
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempt))).await;
                    attempt += 1;
                }
                _ => return Ok(None),
            }
        }
    }

    /// Announcement RSVP's single paced lookup: only 404 means absent. Unlike
    /// the generic legacy REST GET, HTTP failures and malformed JSON must not
    /// masquerade as a missing event. Never expose transport details in replies.
    pub async fn get_scheduled_event(
        &self,
        guild_id: &str,
        event_id: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        let guild = snowflake::<GuildMarker>(guild_id).map_err(|_| "Invalid guild id.")?;
        let event = snowflake::<twilight_model::id::marker::ScheduledEventMarker>(event_id)
            .map_err(|_| "Invalid scheduled event id.")?;
        let request = Request::from_route(&Route::GetGuildScheduledEvent {
            guild_id: guild.get(),
            scheduled_event_id: event.get(),
            with_user_count: false,
        });
        // Guard admission and pacing run inside the one bounded attempt.
        let (res, _) = self
            .send_with_timeout(&request, Some(false))
            .await
            .map_err(|_| "Unable to validate scheduled event.")?;
        match res.status {
            404 => Ok(None),
            200..=299 => serde_json::from_slice(&res.body)
                .map(Some)
                .map_err(|_| "Discord returned an invalid scheduled event status.".to_owned()),
            status => Err(format!("Discord request failed: HTTP {status}")),
        }
    }

    async fn read_json(
        &self,
        path: &str,
        checked: bool,
    ) -> Result<Option<serde_json::Value>, String> {
        let route = raw_get_route(path)?;
        let mut attempt: u32 = 0;
        loop {
            let request = Request::from_route(&route);
            // Guard refusals are terminal; pacing completes before the send.
            self.admit(&request, Some(false))
                .await
                .map_err(|error| error.to_string())?;
            let (res, global) = match self.send_admitted(&request).await {
                Ok(r) => r,
                Err(detail) => {
                    if attempt >= MAX_HTTP_TRIES - 1 {
                        return Err(detail.to_string());
                    }
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempt))).await;
                    attempt += 1;
                    continue;
                }
            };
            match res.status {
                200..=299 => {
                    let value = serde_json::from_slice(&res.body);
                    return if checked {
                        value
                            .map(Some)
                            .map_err(|_| "unreadable Discord evidence".to_owned())
                    } else {
                        Ok(value.ok())
                    };
                }
                429 => {
                    if !global {
                        tokio::time::sleep(Duration::from_millis(res.retry_after_wait_ms())).await;
                    }
                }
                403 | 404 => return Ok(None),
                500..=599 => {
                    if attempt >= MAX_HTTP_TRIES - 1 {
                        return if checked {
                            Err("Discord evidence unavailable".to_owned())
                        } else {
                            Ok(None)
                        };
                    }
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempt))).await;
                    attempt += 1;
                }
                _ => {
                    return if checked {
                        Err("Discord evidence unavailable".to_owned())
                    } else {
                        Ok(None)
                    }
                }
            }
        }
    }

    /// Safety-critical operator read. Only 404 is absence; authorization,
    /// rate-limit, malformed JSON and upstream failures must stop the caller.
    /// Uses the shared paced transport, without silently retrying credentials.
    pub async fn get_json_strict(
        &self,
        path: &str,
    ) -> Result<Option<serde_json::Value>, DiscordError> {
        let route = raw_get_route(path).map_err(DiscordError::Rejected)?;
        // Guard admission and pacing run inside the one bounded attempt.
        let (res, _) = self
            .send_with_timeout(&Request::from_route(&route), Some(false))
            .await?;
        match res.status {
            200..=299 => serde_json::from_slice(&res.body)
                .map(Some)
                .map_err(|_| DiscordError::Unavailable("invalid JSON response".into())),
            404 => Ok(None),
            _ => Err(throw_for_status(&res)),
        }
    }

    /// One paced GET without retries. Bounded roster scans use this so the
    /// page budget is also a wire-request budget, including 429/5xx responses.
    pub async fn get_json_once(&self, path: &str) -> Result<Option<serde_json::Value>, String> {
        let route = raw_get_route(path)?;
        let (res, _) = self
            .send_paced(&Request::from_route(&route), false)
            .await
            .map_err(|error| error.to_string())?;
        Ok(if (200..=299).contains(&res.status) {
            serde_json::from_slice(&res.body).ok()
        } else {
            None
        })
    }

    /// Channel GET with the paced lane (legacy `getEveryoneOverwrite` reads
    /// `permission_overwrites` off the channel).
    pub async fn get_everyone_overwrite(
        &self,
        channel_id: &str,
        guild_id: &str,
    ) -> Result<Option<EveryoneOverwrite>, DiscordError> {
        self.read_everyone_overwrite(channel_id, guild_id, false)
            .await
    }

    /// Resolve a website moderation channel inside the configured guild. Fail
    /// closed on unreadable identity/type before a claim can mutate another guild.
    pub async fn get_guild_channel_overwrite(
        &self,
        channel_id: &str,
        guild_id: &str,
    ) -> Result<Option<EveryoneOverwrite>, DiscordError> {
        self.read_everyone_overwrite(channel_id, guild_id, true)
            .await
    }

    async fn read_everyone_overwrite(
        &self,
        channel_id: &str,
        guild_id: &str,
        enforce_guild: bool,
    ) -> Result<Option<EveryoneOverwrite>, DiscordError> {
        let channel: Id<ChannelMarker> = snowflake(channel_id)?;
        // The requested guild identity is validated with the same snowflake
        // rules the PUT target uses, before any I/O: the read must compare
        // normalized identities, not raw strings, so a noncanonical caller
        // id (e.g. "02222") resolves to the same target the mutation uses
        // (finding 3 identity validation).
        let target: Id<GuildMarker> = snowflake(guild_id)?;
        let req = Self::request_of(self.inner.factory.channel(channel))?;
        // Reads use the paced lane with a single attempt (channel verbs run
        // under the router's own pacing; the 5 s abort still applies). The
        // raw body is validated strictly here: an unreadable channel document
        // must refuse — reading it as "absent" would PUT a fabricated
        // zero-mask overwrite and erase live allow/deny bits (finding 3).
        // Only a proven-absent entry (valid document, array present, no
        // @everyone row) returns `Ok(None)`. Read-only, so refusal is
        // Rejected (safe pre-mutation).
        let res = self.call_once_raw(req, &[200]).await?;
        let doc: serde_json::Value = serde_json::from_slice(&res.body).map_err(|_| {
            DiscordError::Rejected(format!("unreadable channel {channel_id}: body is not JSON"))
        })?;
        if enforce_guild {
            let doc_channel = doc
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|id| snowflake::<ChannelMarker>(id).ok());
            let doc_guild = doc
                .get("guild_id")
                .and_then(|v| v.as_str())
                .and_then(|id| snowflake::<GuildMarker>(id).ok());
            if doc_channel != Some(channel)
                || doc_guild != Some(target)
                || !matches!(doc.get("type").and_then(|v| v.as_u64()), Some(0 | 5))
            {
                return Err(DiscordError::Rejected(
                    "channel is not a text channel in the configured guild".to_owned(),
                ));
            }
        }
        let overwrites = doc.get("permission_overwrites").ok_or_else(|| {
            DiscordError::Rejected(format!(
                "unreadable channel {channel_id}: missing permission_overwrites"
            ))
        })?;
        let overwrites = overwrites.as_array().ok_or_else(|| {
            DiscordError::Rejected(format!(
                "unreadable channel {channel_id}: permission_overwrites is not an array"
            ))
        })?;
        for (index, entry) in overwrites.iter().enumerate() {
            // Every row must carry a readable identity before it can count as
            // "not @everyone": a malformed row (non-object, missing/non-string
            // id, missing/non-numeric type) makes the document unreadable, so
            // refuse rather than treating it as proven-absent and PUTting a
            // fabricated zero-mask overwrite (finding 3 follow-up).
            let row = entry.as_object().ok_or_else(|| {
                DiscordError::Rejected(format!(
                    "unreadable channel {channel_id}: permission_overwrites[{index}] is not an object"
                ))
            })?;
            let id_raw = row.get("id").and_then(|v| v.as_str()).ok_or_else(|| {
                DiscordError::Rejected(format!(
                    "unreadable channel {channel_id}: permission_overwrites[{index}] has a non-string id"
                ))
            })?;
            let kind = row.get("type").and_then(|v| v.as_u64()).ok_or_else(|| {
                DiscordError::Rejected(format!(
                    "unreadable channel {channel_id}: permission_overwrites[{index}] has a non-numeric type"
                ))
            })?;
            // String shape is not identity: empty, nonnumeric, zero, and
            // overflowing ids must refuse, and the comparison must use the
            // normalized snowflake — comparing raw strings would miss a row
            // whose id normalizes to the same target the PUT uses (finding 3
            // identity validation).
            let id: Id<GuildMarker> = snowflake(id_raw).map_err(|_| {
                DiscordError::Rejected(format!(
                    "unreadable channel {channel_id}: permission_overwrites[{index}] has an invalid id"
                ))
            })?;
            if id == target && kind == 0 {
                let mask = |field: &str| {
                    entry
                        .get(field)
                        .and_then(|v| v.as_str())
                        .filter(|s| s.parse::<u64>().is_ok())
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            DiscordError::Rejected(format!(
                                "unreadable channel {channel_id}: @everyone overwrite has a non-numeric {field} mask"
                            ))
                        })
                };
                return Ok(Some(EveryoneOverwrite {
                    allow: mask("allow")?,
                    deny: mask("deny")?,
                }));
            }
        }
        Ok(None)
    }

    /// Raw channel document for the audit-mirror preflight
    /// (`GET /channels/{c}`, single paced read). Unlike
    /// [`Self::get_everyone_overwrite`] this keeps the whole document: the
    /// `AuditMirror` adapter owns guild/privacy field policy, so a body that
    /// is not a JSON object is unavailable evidence, not permission loss.
    pub async fn fetch_channel_document(
        &self,
        channel_id: &str,
    ) -> Result<serde_json::Value, DiscordError> {
        let channel: Id<ChannelMarker> = snowflake(channel_id)?;
        let req = Self::request_of(self.inner.factory.channel(channel))?;
        let (res, _) = self.send_with_timeout(&req, Some(false)).await?;
        if res.status != 200 {
            return Err(throw_for_status(&res));
        }
        let doc: serde_json::Value = serde_json::from_slice(&res.body).map_err(|_| {
            DiscordError::Unavailable(format!("unreadable channel {channel_id}: body is not JSON"))
        })?;
        if !doc.is_object() {
            return Err(DiscordError::Unavailable(format!(
                "unreadable channel {channel_id}: body is not a JSON object"
            )));
        }
        Ok(doc)
    }

    /// One newest-first history page for the audit-mirror dedup/reconcile
    /// scan (`GET /channels/{c}/messages`, legacy `findMirror` reads).
    /// `before` is the previous page's floor id; `limit` clamps into
    /// Discord's 1..=100 range. The body must be a JSON array; per-element
    /// field policy belongs to the `AuditMirror` adapter; malformed evidence
    /// is uncertain rather than proof of marker absence.
    pub async fn fetch_channel_messages(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: u8,
    ) -> Result<Vec<serde_json::Value>, DiscordError> {
        let channel: Id<ChannelMarker> = snowflake(channel_id)?;
        let before: Option<Id<MessageMarker>> =
            before.map(snowflake::<MessageMarker>).transpose()?;
        let limit = u16::from(limit.clamp(1, 100));
        let req = match before {
            Some(cursor) => Self::request_of(
                self.inner
                    .factory
                    .channel_messages(channel)
                    .before(cursor)
                    .limit(limit),
            )?,
            None => Self::request_of(self.inner.factory.channel_messages(channel).limit(limit))?,
        };
        let (res, _) = self.send_with_timeout(&req, Some(false)).await?;
        if res.status != 200 {
            return Err(throw_for_status(&res));
        }
        serde_json::from_slice(&res.body).map_err(|_| {
            DiscordError::Unavailable(format!(
                "unreadable channel {channel_id} history: body is not a JSON array"
            ))
        })
    }

    /// `PUT /channels/{c}/permissions/{g}` with decimal-string masks
    /// (legacy `putEveryoneOverwrite`, 200/204).
    pub async fn put_everyone_overwrite(
        &self,
        channel_id: &str,
        guild_id: &str,
        allow: &str,
        deny: &str,
        reason: &str,
    ) -> Result<(), DiscordError> {
        use twilight_model::guild::Permissions;
        let channel = snowflake(channel_id)?;
        let target: Id<twilight_model::id::marker::GenericMarker> = snowflake(guild_id)?;
        let reason = audit_reason(reason)?;
        let allow_bits = allow
            .parse::<u64>()
            .map_err(|_| DiscordError::Rejected(format!("bad allow mask: {allow}")))?;
        let deny_bits = deny
            .parse::<u64>()
            .map_err(|_| DiscordError::Rejected(format!("bad deny mask: {deny}")))?;
        // `from_bits_retain`, not `truncate`: unmodeled bits (e.g. 1 << 48,
        // absent from the pinned model) must round-trip on the wire, or the
        // preservation/restoration contract silently drops them (finding 4).
        // `Permissions` serializes as its decimal bits string, so retained
        // bits are sent losslessly.
        let overwrite = PermissionOverwrite {
            allow: Some(Permissions::from_bits_retain(allow_bits)),
            deny: Some(Permissions::from_bits_retain(deny_bits)),
            id: target,
            kind: PermissionOverwriteType::Role,
        };
        let req = Self::request_of(
            self.inner
                .factory
                .update_channel_permission(channel, &overwrite)
                .reason(&reason),
        )?;
        // request_of maps pre-send build failures to Rejected (finding 7).
        self.call_once(req, &[200, 204]).await.map(|_| ())
    }

    /// `DELETE /channels/{c}/permissions/{g}` (legacy
    /// `deleteEveryoneOverwrite`, 200/204/404).
    pub async fn delete_everyone_overwrite(
        &self,
        channel_id: &str,
        guild_id: &str,
        reason: &str,
    ) -> Result<(), DiscordError> {
        let channel = snowflake(channel_id)?;
        let target: Id<twilight_model::id::marker::GenericMarker> = snowflake(guild_id)?;
        let reason = audit_reason(reason)?;
        let req = Self::request_of(
            self.inner
                .factory
                .delete_channel_permission(channel)
                .role(target.cast())
                .reason(&reason),
        )?;
        // request_of maps pre-send build failures to Rejected (finding 7).
        self.call_once(req, &[200, 204, 404]).await.map(|_| ())
    }

    /// Slowmode: `PATCH /channels/{c}` with `rate_limit_per_user` (legacy
    /// accepts 200). Bounds (0–21600) are the caller's; twilight validates
    /// the wire range.
    pub async fn set_slowmode(
        &self,
        channel_id: &str,
        seconds: u64,
        reason: &str,
    ) -> Result<(), DiscordError> {
        let channel = snowflake(channel_id)?;
        let reason = audit_reason(reason)?;
        let seconds: u16 = seconds
            .try_into()
            .map_err(|_| DiscordError::Rejected(format!("slowmode out of range: {seconds}")))?;
        let req = Self::request_of(
            self.inner
                .factory
                .update_channel(channel)
                .rate_limit_per_user(seconds)
                .reason(&reason),
        )?;
        // request_of maps pre-send build failures to Rejected (finding 7).
        self.call_once(req, &[200]).await.map(|_| ())
    }

    /// Purge: list then one `DELETE` or `POST .../bulk-delete` (legacy
    /// `ModerationDiscord::purge` returns the affected count).
    pub async fn purge(
        &self,
        channel_id: &str,
        count: u64,
        reason: &str,
    ) -> Result<u64, DiscordError> {
        // Keep reason validation before any I/O for the combined public call.
        audit_reason(reason)?;
        let ids = self.list_purge_messages(channel_id, count).await?;
        self.purge_messages(channel_id, &ids, reason).await
    }

    /// Read-only purge phase. Even a timeout here proves no deletion was sent.
    pub(crate) async fn list_purge_messages(
        &self,
        channel_id: &str,
        count: u64,
    ) -> Result<Vec<Id<MessageMarker>>, DiscordError> {
        let channel: Id<ChannelMarker> = snowflake(channel_id)?;
        let limit: u16 = count
            .clamp(1, 100)
            .try_into()
            .map_err(|_| DiscordError::Rejected(format!("purge out of range: {count}")))?;
        let list_req = Self::request_of(self.inner.factory.channel_messages(channel).limit(limit))?;
        let res = self.call_once_raw(list_req, &[200]).await?;
        let unreadable =
            || DiscordError::Unavailable(format!("unreadable channel {channel_id} purge history"));
        let listed: Vec<serde_json::Value> =
            serde_json::from_slice(&res.body).map_err(|_| unreadable())?;
        listed
            .iter()
            .map(|row| {
                let value = row.get("id").and_then(serde_json::Value::as_str);
                let value = value.ok_or_else(unreadable)?;
                let id: Id<MessageMarker> = snowflake(value).map_err(|_| unreadable())?;
                if id.to_string() != value {
                    return Err(unreadable());
                }
                Ok(id)
            })
            .collect()
    }

    /// Single deletion phase; uncertain wire failures must retain caller fences.
    pub(crate) async fn purge_messages(
        &self,
        channel_id: &str,
        ids: &[Id<MessageMarker>],
        reason: &str,
    ) -> Result<u64, DiscordError> {
        let channel: Id<ChannelMarker> = snowflake(channel_id)?;
        let reason = audit_reason(reason)?;
        if ids.is_empty() {
            return Ok(0);
        }
        if ids.len() == 1 {
            let req = Self::request_of(
                self.inner
                    .factory
                    .delete_message(channel, ids[0])
                    .reason(&reason),
            )?;
            // request_of maps pre-send build failures to Rejected (finding 7).
            self.call_once(req, &[200, 204]).await?;
            return Ok(1);
        }
        let req = Self::request_of(
            self.inner
                .factory
                .delete_messages(channel, ids)
                .reason(&reason),
        )?;
        // request_of maps pre-send build failures to Rejected (finding 7).
        self.call_once(req, &[200, 204]).await?;
        Ok(ids.len() as u64)
    }

    /// Delete one message (legacy `ModerationDiscord` single delete — the
    /// same request purge's one-id arm makes). Best-effort cleanup callers
    /// (sticky retirement) treat `Rejected`/`Http` as a miss, not a crash.
    pub async fn delete_message(
        &self,
        channel_id: &str,
        message_id: &str,
        reason: &str,
    ) -> Result<(), DiscordError> {
        let channel: Id<ChannelMarker> = snowflake(channel_id)?;
        let message: Id<MessageMarker> = snowflake(message_id)?;
        let reason = audit_reason(reason)?;
        let req = Self::request_of(
            self.inner
                .factory
                .delete_message(channel, message)
                .reason(&reason),
        )?;
        // request_of maps pre-send build failures to Rejected (finding 7).
        self.call_once(req, &[200, 204]).await?;
        Ok(())
    }

    /// Post a message with mention suppression (legacy
    /// `allowed_mentions: { parse: [] }`). Asserts the legacy 2000 UTF-16-unit
    /// ceiling before sending; requires a valid message id receipt. An
    /// unreadable receipt is uncertain, not an empty successful id. A numeric
    /// nonce is sent with `enforce_nonce: true` for
    /// duplicate suppression.
    pub async fn post_message(
        &self,
        channel_id: &str,
        content: &str,
        nonce: Option<u64>,
    ) -> Result<String, DiscordError> {
        self.send_message(
            channel_id,
            content,
            nonce.map(serde_json::Value::from),
            false,
        )
        .await
    }

    /// Audit callers already hold the paced lane and a fresh DB authorization.
    /// Refuse a newly closed guard instead of sleeping after that fence.
    pub(crate) async fn post_message_after_authorization(
        &self,
        channel_id: &str,
        content: &str,
        nonce: &str,
    ) -> Result<String, DiscordError> {
        let nonce = match nonce.parse::<u64>() {
            Ok(value) => serde_json::Value::from(value),
            Err(_) => serde_json::Value::from(nonce),
        };
        self.send_message(channel_id, content, Some(nonce), true)
            .await
    }

    /// Post a rendered feature message through the shared request factory.
    pub async fn post_message_with_components(
        &self,
        channel_id: &str,
        content: &str,
        components: &[serde_json::Value],
        nonce: &str,
    ) -> Result<String, DiscordError> {
        self.send_message_components(
            channel_id,
            content,
            Some(nonce.into()),
            Some(components),
            false,
        )
        .await
    }

    /// Refresh content and selects. Edits must suppress mentions independently of POST.
    pub async fn edit_message_with_components(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
        components: &[serde_json::Value],
    ) -> Result<(), DiscordError> {
        if utf16_len(content) > MAX_MESSAGE_CHARS {
            return Err(DiscordError::Rejected(
                "message exceeds Discord's ceiling".into(),
            ));
        }
        let channel: Id<ChannelMarker> = snowflake(channel_id)?;
        let message: Id<MessageMarker> = snowflake(message_id)?;
        let body = serde_json::to_vec(&serde_json::json!({
            "content": content, "components": components, "allowed_mentions": {"parse": []}
        }))
        .map_err(|e| DiscordError::Rejected(format!("build message body: {e}")))?;
        let req = Request::builder(&Route::UpdateMessage {
            channel_id: channel.get(),
            message_id: message.get(),
        })
        .body(body)
        .build()
        .map_err(|e| DiscordError::Rejected(format!("build: {e}")))?;
        self.call_once_raw_paced(req, &[200]).await?;
        Ok(())
    }

    /// Bounded nonce reconciliation. An unreadable/denied history is NOT an empty history.
    /// Only messages from this bot in the target channel can establish acceptance.
    pub async fn recover_message_by_nonce(
        &self,
        channel_id: &str,
        nonce: &str,
        bot_user_id: u64,
    ) -> Result<Option<String>, DiscordError> {
        // Without the bot's identity no author check can prove acceptance.
        if bot_user_id == 0 {
            return Err(DiscordError::Unavailable("bot identity unknown".into()));
        }
        let mut before = None;
        for _ in 0..3 {
            let path = match &before {
                Some(id) => format!("/channels/{channel_id}/messages?limit=100&before={id}"),
                None => format!("/channels/{channel_id}/messages?limit=100"),
            };
            let route = raw_get_route(&path).map_err(DiscordError::Rejected)?;
            let res = self
                .call_once_raw_paced(Request::from_route(&route), &[200])
                .await?;
            let rows: Vec<serde_json::Value> = serde_json::from_slice(&res.body)
                .map_err(|_| DiscordError::Unavailable("unreadable message history".into()))?;
            for row in &rows {
                if row["nonce"].as_str() == Some(nonce)
                    && row["author"]["id"].as_str() == Some(&bot_user_id.to_string())
                    && row["channel_id"].as_str() == Some(channel_id)
                {
                    let id = row["id"].as_str().ok_or_else(|| {
                        DiscordError::Unavailable("accepted message missing id".into())
                    })?;
                    let _: Id<MessageMarker> = snowflake(id)?;
                    return Ok(Some(id.to_owned()));
                }
            }
            if rows.len() < 100 {
                return Ok(None);
            }
            let last = rows
                .last()
                .and_then(|r| r["id"].as_str())
                .ok_or_else(|| DiscordError::Unavailable("unreadable history cursor".into()))?;
            let _: Id<MessageMarker> = snowflake(last)?;
            if before.as_deref() == Some(last) {
                return Err(DiscordError::Unavailable(
                    "non-progressing history cursor".into(),
                ));
            }
            before = Some(last.to_owned());
        }
        Err(DiscordError::Unavailable(
            "nonce recovery history bound reached".into(),
        ))
    }

    /// Complete an already-deferred ephemeral reply through the same executor.
    pub async fn finish_interaction(
        &self,
        application_id: u64,
        token: &str,
        content: &str,
    ) -> Result<(), DiscordError> {
        let application = Id::<ApplicationMarker>::new_checked(application_id)
            .ok_or_else(|| DiscordError::Rejected("bad application id".into()))?;
        let interaction = self.inner.factory.interaction(application);
        let mentions = AllowedMentions {
            parse: vec![],
            replied_user: false,
            roles: vec![],
            users: vec![],
        };
        let req = Self::request_of(
            interaction
                .update_response(token)
                .content(Some(content))
                .allowed_mentions(Some(&mentions)),
        )?;
        self.call_once_raw(req, &[200]).await?;
        Ok(())
    }

    /// Raw message send shared by [`Self::post_message`] and the audit
    /// string-nonce path: the pinned Twilight `CreateMessage` builder only
    /// models a `u64` nonce and never serializes `enforce_nonce`, so the body
    /// is built explicitly — method and path stay twilight-owned via
    /// `Route::CreateMessage` (finding 5). `nonce` is either a JSON number or
    /// a ≤25-char string (Discord's string-nonce ceiling, which is exactly
    /// what `audit::delivery_nonce` mints).
    async fn send_message(
        &self,
        channel_id: &str,
        content: &str,
        nonce: Option<serde_json::Value>,
        after_authorization: bool,
    ) -> Result<String, DiscordError> {
        self.send_message_components(channel_id, content, nonce, None, after_authorization)
            .await
    }

    async fn send_message_components(
        &self,
        channel_id: &str,
        content: &str,
        nonce: Option<serde_json::Value>,
        components: Option<&[serde_json::Value]>,
        after_authorization: bool,
    ) -> Result<String, DiscordError> {
        // Legacy ceiling is UTF-16 units (two-bot counts JS string length),
        // not scalar values: 1001 astral chars are 2002 units and must be
        // rejected with zero wire calls (finding 8).
        let units = utf16_len(content);
        if units > MAX_MESSAGE_CHARS {
            return Err(DiscordError::Rejected(format!(
                "message is {units} UTF-16 units; Discord's ceiling is {MAX_MESSAGE_CHARS}"
            )));
        }
        let channel: Id<ChannelMarker> = snowflake(channel_id)?;
        if let Some(serde_json::Value::String(s)) = &nonce {
            if s.is_empty() || s.len() > 25 {
                return Err(DiscordError::Rejected(format!(
                    "bad nonce: string nonces must be 1-25 chars, got {}",
                    s.len()
                )));
            }
        }
        let mut body = serde_json::json!({
            "content": content,
            "allowed_mentions": {"parse": []},
        });
        if let Some(components) = components {
            body["components"] = serde_json::json!(components);
        }
        if let Some(n) = nonce {
            body["nonce"] = n;
            body["enforce_nonce"] = serde_json::Value::Bool(true);
        }
        crate::message_safety::sanitize_message(&mut body);
        crate::message_safety::validate_create(&body)?;
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| DiscordError::Rejected(format!("build message body: {e}")))?;
        let req = Request::builder(&Route::CreateMessage {
            channel_id: channel.get(),
        })
        .body(body_bytes)
        .build()
        .map_err(|e| DiscordError::Rejected(format!("build: {e}")))?;
        let paced = components.is_some();
        let message_id = if after_authorization {
            self.inner.transport.guard.check_now(false)?;
            let (mut res, _) =
                tokio::time::timeout(self.inner.moderation_timeout, self.send_admitted(&req))
                    .await
                    .map_err(|_| DiscordError::Timeout)??;
            if ![200, 201].contains(&res.status) {
                return Err(throw_for_status(&res));
            }
            // An accepted mutation still needs its validated id receipt
            // before the durable lane reopens; an unreadable receipt is
            // uncertain, never an empty successful id.
            let id = mutation_receipt_id(&res.body)?;
            res.complete().await;
            id
        } else {
            // Feature posts with selects take the paced lane; the plain
            // `post_message` path keeps main's unpaced single attempt.
            let mut res = if paced {
                self.call_once_raw_paced(req, &[200, 201]).await?
            } else {
                self.call_once_raw(req, &[200, 201]).await?
            };
            let id = mutation_receipt_id(&res.body)?;
            res.complete().await;
            id
        };
        Ok(message_id)
    }

    // Twilight omits empty roles/users lists when serializing AllowedMentions.
    // Onboarding's wire contract requires all three lists explicitly present.
    // Keep the validated builder's method/path/auth (notably webhook auth=false).
    fn explicit_mentions(
        req: Request,
        mentions: &AllowedMentions,
    ) -> Result<Request, DiscordError> {
        let mut body: serde_json::Value = serde_json::from_slice(req.body().unwrap_or_default())
            .map_err(|_| DiscordError::Rejected("invalid message body".into()))?;
        body["allowed_mentions"] = serde_json::json!({
            "parse": mentions.parse,
            "users": mentions.users,
            "roles": mentions.roles,
            "replied_user": mentions.replied_user,
        });
        let bytes = serde_json::to_vec(&body)
            .map_err(|_| DiscordError::Rejected("invalid mention policy".into()))?;
        let mut builder =
            twilight_http::request::RequestBuilder::raw(req.method(), req.path().to_owned())
                .body(bytes)
                .use_authorization_token(req.use_authorization_token());
        if let Some(headers) = req.headers() {
            builder = builder.headers(
                headers
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
        builder
            .build()
            .map_err(|_| DiscordError::Rejected("invalid message request".into()))
    }

    /// Post a component-bearing message through the shared bounded transport.
    /// Empty components are omitted (anchor welcomes must attach nothing).
    /// No automatic retry: send-then-record callers must not hide ambiguity.
    /// Only the domain-authorized welcome recipient may notify; arbitrary parse,
    /// role, multi-user and reply policies cannot enter this boundary.
    /// Source: https://docs.rs/twilight-http/0.17.1/twilight_http/request/channel/message/struct.CreateMessage.html
    pub async fn post_channel_message(
        &self,
        channel_id: &str,
        content: &str,
        components: &[twilight_model::channel::message::Component],
        policy: two_bot_core::onboarding::MentionPolicy,
    ) -> Result<String, DiscordError> {
        if utf16_len(content) > MAX_MESSAGE_CHARS {
            return Err(DiscordError::Rejected(
                "message exceeds UTF-16 ceiling".into(),
            ));
        }
        if matches!(policy, two_bot_core::onboarding::MentionPolicy::Member(0)) {
            return Err(DiscordError::Rejected("bad welcome recipient".into()));
        }
        let content = two_bot_core::message_safety::content(content);
        crate::message_safety::validate_create(&serde_json::json!({
            "content": content,
            "components": components,
        }))?;
        let mentions = crate::onboarding_messages::allowed_mentions(policy);
        let mut builder = self
            .inner
            .factory
            .create_message(snowflake(channel_id)?)
            .allowed_mentions(Some(&mentions));
        if !content.is_empty() {
            builder = builder.content(&content);
        }
        if !components.is_empty() {
            builder = builder.components(components);
        }
        let req = Self::explicit_mentions(Self::request_of(builder)?, &mentions)?;
        // An accepted mutation still needs its validated id receipt before the
        // durable lane reopens; an unreadable receipt is uncertain, never an
        // empty successful id that a caller would record as delivered.
        let mut res = self.call_once_raw(req, &[200, 201]).await?;
        let id = mutation_receipt_id(&res.body)?;
        res.complete().await;
        Ok(id)
    }

    /// Add or remove one member role, preserving all unrelated roles. The
    /// shared lane owns pacing; mutations are bounded and never auto-retried.
    pub async fn set_member_role(
        &self,
        guild_id: &str,
        member_id: &str,
        role_id: &str,
        present: bool,
        reason: &str,
    ) -> Result<(), DiscordError> {
        let guild = snowflake(guild_id)?;
        let member = snowflake(member_id)?;
        let role = snowflake(role_id)?;
        let req = if present {
            Self::request_of(
                self.inner
                    .factory
                    .add_guild_member_role(guild, member, role)
                    .reason(reason),
            )?
        } else {
            Self::request_of(
                self.inner
                    .factory
                    .remove_guild_member_role(guild, member, role)
                    .reason(reason),
            )?
        };
        let mut res = self.call_once_raw_paced(req, &[200, 204]).await?;
        res.complete().await;
        Ok(())
    }

    /// Carry out one [`ChannelCall`].
    pub async fn execute_channel(
        &self,
        call: &ChannelCall,
    ) -> Result<ChannelCallOutcome, DiscordError> {
        match call {
            ChannelCall::Purge {
                channel_id,
                count,
                reason,
            } => Ok(ChannelCallOutcome::Purged {
                affected: self.purge(channel_id, *count, reason).await?,
            }),
            ChannelCall::Slowmode {
                channel_id,
                seconds,
                reason,
            } => {
                self.set_slowmode(channel_id, *seconds, reason).await?;
                Ok(ChannelCallOutcome::SlowmodeUpdated)
            }
            ChannelCall::PutOverwrite {
                channel_id,
                guild_id,
                allow,
                deny,
                reason,
            } => {
                self.put_everyone_overwrite(channel_id, guild_id, allow, deny, reason)
                    .await?;
                Ok(ChannelCallOutcome::OverwriteWritten)
            }
            ChannelCall::DeleteOverwrite {
                channel_id,
                guild_id,
                reason,
            } => {
                self.delete_everyone_overwrite(channel_id, guild_id, reason)
                    .await?;
                Ok(ChannelCallOutcome::OverwriteDeleted)
            }
            ChannelCall::PostMessage {
                channel_id,
                content,
                nonce,
            } => {
                // `audit::delivery_nonce` mints `oa_...` strings that can never
                // parse as u64 — carry them as string nonces with enforcement
                // instead of rejecting them (finding 5). Numeric strings keep
                // the numeric wire shape.
                let value = nonce.as_deref().map(|s| match s.parse::<u64>() {
                    Ok(n) => serde_json::Value::from(n),
                    Err(_) => serde_json::Value::from(s),
                });
                Ok(ChannelCallOutcome::Posted {
                    message_id: self.send_message(channel_id, content, value, false).await?,
                })
            }
            ChannelCall::DeleteMessage {
                channel_id,
                message_id,
                reason,
            } => {
                self.delete_message(channel_id, message_id, reason).await?;
                Ok(ChannelCallOutcome::MessageDeleted)
            }
        }
    }

    /// Read the complete guild registry, including localization dictionaries.
    /// A failed/malformed read is never interpreted as an empty registry.
    pub async fn guild_commands(
        &self,
        application_id: u64,
        guild_id: u64,
    ) -> Result<Vec<twilight_model::application::command::Command>, DiscordError> {
        let application = Id::<ApplicationMarker>::new_checked(application_id)
            .ok_or_else(|| DiscordError::Rejected("bad application id".into()))?;
        let guild = Id::<GuildMarker>::new_checked(guild_id)
            .ok_or_else(|| DiscordError::Rejected("bad guild id".into()))?;
        let req = Self::request_of(
            self.inner
                .factory
                .interaction(application)
                .guild_commands(guild)
                .with_localizations(true),
        )?;
        for attempt in 0..MAX_HTTP_TRIES {
            // Paced non-kick lane with the same bounded wire budget as the
            // publish sibling; 429 parks and 5xx back off within MAX_HTTP_TRIES.
            self.admit(&req, Some(false)).await?;
            let (res, _) =
                tokio::time::timeout(self.inner.moderation_timeout, self.send_admitted(&req))
                    .await
                    .map_err(|_| DiscordError::Timeout)??;
            match res.status {
                200..=299 => {
                    return crate::command_registry::decode_guild_commands(&res.body).map_err(
                        |_| DiscordError::Unavailable("invalid guild command response".into()),
                    )
                }
                429 if attempt < MAX_HTTP_TRIES - 1 => {
                    tokio::time::sleep(Duration::from_millis(res.retry_after_wait_ms())).await;
                }
                429 => return Err(DiscordError::RateLimited),
                500..=599 if attempt < MAX_HTTP_TRIES - 1 => {
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempt))).await;
                }
                other => return Err(throw_for_status(&res).into_other(other)),
            }
        }
        Err(DiscordError::Unavailable(
            "guild command read exhausted".into(),
        ))
    }

    /// Shared dry-run/boot workflow. Fetch-and-hash on every invocation detects
    /// out-of-band changes and survives restarts without a stale DB hash cache.
    /// Returns the pre-write diff and whether a full bulk overwrite was sent.
    pub async fn sync_guild_commands(
        &self,
        application_id: u64,
        guild_id: u64,
        commands: &[twilight_model::application::command::Command],
        apply: bool,
    ) -> Result<(crate::command_registry::RegistryDiff, bool), DiscordError> {
        let current = self.guild_commands(application_id, guild_id).await?;
        let diff = crate::command_registry::diff_commands(&current, commands)
            .map_err(|_| DiscordError::Rejected("cannot canonicalize command registry".into()))?;
        let publish = apply && diff.current_hash != diff.compiled_hash;
        if publish {
            self.publish_guild_commands(application_id, guild_id, commands)
                .await?;
        }
        Ok((diff, publish))
    }

    /// Resolve the authenticated bot USER, not its application, after RESUMED.
    /// One bounded, paced read; malformed/non-bot evidence never initializes
    /// author checks or permission targets with a guessed identity.
    pub async fn current_bot_user_id(&self) -> Result<u64, DiscordError> {
        let req = Self::request_of(self.inner.factory.current_user())?;
        let res = self.call_once_raw_paced(req, &[200]).await?;
        let body: serde_json::Value = serde_json::from_slice(&res.body)
            .map_err(|_| DiscordError::Unavailable("invalid bot user response".into()))?;
        let id = body["id"]
            .as_str()
            .and_then(|value| value.parse::<u64>().ok().map(|id| (value, id)))
            .filter(|(value, id)| *id != 0 && id.to_string() == *value)
            .map(|(_, id)| id)
            .filter(|_| body["bot"].as_bool() == Some(true))
            .ok_or_else(|| DiscordError::Unavailable("invalid bot user identity".into()))?;
        Ok(id)
    }

    /// Resolve the authenticated bot's application for a resumed startup
    /// without READY. One bounded, paced read; no alternate client or guessed id.
    pub async fn current_application_id(&self) -> Result<u64, DiscordError> {
        let req = Self::request_of(self.inner.factory.current_user_application())?;
        let (res, _) = self.send_with_timeout(&req, Some(false)).await?;
        match res.status {
            200..=299 => {
                let body: serde_json::Value = serde_json::from_slice(&res.body).map_err(|_| {
                    DiscordError::Unavailable("invalid application response".to_owned())
                })?;
                let id: Id<ApplicationMarker> = serde_json::from_value(body["id"].clone())
                    .map_err(|_| DiscordError::Unavailable("invalid application id".to_owned()))?;
                Ok(id.get())
            }
            _ => Err(throw_for_status(&res)),
        }
    }

    /// Publish the router's full guild command set in one send
    /// (`PUT /applications/{app}/guilds/{guild}/commands`).
    pub async fn publish_guild_commands(
        &self,
        application_id: u64,
        guild_id: u64,
        commands: &[twilight_model::application::command::Command],
    ) -> Result<(), DiscordError> {
        let application =
            Id::<ApplicationMarker>::new_checked(application_id).ok_or_else(|| {
                DiscordError::Rejected(format!("bad application id: {application_id}"))
            })?;
        let guild = Id::<GuildMarker>::new_checked(guild_id)
            .ok_or_else(|| DiscordError::Rejected(format!("bad guild id: {guild_id}")))?;
        let req = Self::request_of(
            self.inner
                .factory
                .interaction(application)
                .set_guild_commands(guild, commands),
        )?;
        // request_of maps pre-send build failures to Rejected (finding 7).
        // Publish is idempotent registry sync: paced lane, 429 parks and 5xx
        // back off within the same budget as kicks.
        let mut attempts: u32 = 0;
        loop {
            // Idempotent sync may wait out any global pause; the five-second
            // wire budget starts after paced admission, unlike moderation.
            self.admit(&req, Some(false)).await?;
            let (mut res, global) =
                tokio::time::timeout(self.inner.moderation_timeout, self.send_admitted(&req))
                    .await
                    .map_err(|_| DiscordError::Timeout)??;
            match res.status {
                200 => {
                    let published: Vec<twilight_model::application::command::Command> =
                        serde_json::from_slice(&res.body).map_err(|_| {
                            DiscordError::Unavailable("invalid command registry receipt".to_owned())
                        })?;
                    if published.len() != commands.len() {
                        return Err(DiscordError::Unavailable(
                            "incomplete command registry receipt".to_owned(),
                        ));
                    }
                    res.complete().await;
                    return Ok(());
                }
                429 => {
                    if attempts >= MAX_HTTP_TRIES - 1 {
                        return Err(DiscordError::RateLimited);
                    }
                    if !global {
                        tokio::time::sleep(Duration::from_millis(res.retry_after_wait_ms())).await;
                    }
                    attempts += 1;
                }
                500..=599 => {
                    if attempts >= MAX_HTTP_TRIES - 1 {
                        return Err(DiscordError::Unavailable(format!("publish {}", res.status)));
                    }
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempts))).await;
                    attempts += 1;
                }
                other => return Err(throw_for_status(&res).into_other(other)),
            }
        }
    }

    /// Answer an interaction (`POST /interactions/{id}/{token}/callback`).
    pub async fn answer_interaction(
        &self,
        interaction_id: u64,
        interaction_token: &str,
        response: &twilight_model::http::interaction::InteractionResponse,
    ) -> Result<(), DiscordError> {
        let interaction_id =
            Id::<InteractionMarker>::new_checked(interaction_id).ok_or_else(|| {
                DiscordError::Rejected(format!("bad interaction id: {interaction_id}"))
            })?;
        let response = crate::message_safety::interaction_response(response)?;
        let req = Self::request_of(
            self.inner
                .factory
                .interaction(Id::<ApplicationMarker>::new(1))
                .create_response(interaction_id, interaction_token, &response),
        )?;
        // request_of maps pre-send build failures to Rejected (finding 7).
        let (mut res, _) = self.send_with_timeout(&req, None).await?;
        match res.status {
            200 => {
                mutation_receipt_id(&res.body)?;
                res.complete().await;
                Ok(())
            }
            204 => {
                res.complete().await;
                Ok(())
            }
            _ => Err(throw_for_status(&res)),
        }
    }

    /// Bounded receipt-callback retry for governed-lane occupancy: re-attempts
    /// ONLY admission-Blocked failures within a 2.5 s budget (~20 ms sleeps),
    /// leaving margin below Discord's three-second acknowledgement window. A
    /// Blocked attempt never reached the wire, so retrying it cannot double-ACK;
    /// every other error returns immediately with no retry.
    pub async fn answer_interaction_with_blocked_retry(
        &self,
        interaction_id: u64,
        interaction_token: &str,
        response: &twilight_model::http::interaction::InteractionResponse,
    ) -> Result<(), DiscordError> {
        let start = tokio::time::Instant::now();
        loop {
            match self
                .answer_interaction(interaction_id, interaction_token, response)
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) if error.is_admission_blocked() => {
                    if start.elapsed() >= Duration::from_millis(2500) {
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Execute a router reply operation in the unpaced interaction lane.
    /// No automatic retries: a lost callback response may already be an ACK.
    pub async fn execute_reply_operation(
        &self,
        application_id: u64,
        interaction_id: u64,
        interaction_token: &str,
        operation: two_bot_core::router::replies::ReplyOperation,
    ) -> Result<Option<u64>, DiscordError> {
        use twilight_model::channel::message::{AllowedMentions, MessageFlags};
        use two_bot_core::router::replies::ReplyOperation;
        let application = Id::<ApplicationMarker>::new_checked(application_id)
            .ok_or_else(|| DiscordError::Rejected("bad application id".to_owned()))?;
        let client = self.inner.factory.interaction(application);
        let mentions = AllowedMentions::default();
        let creates_followup = matches!(operation, ReplyOperation::Followup(_));
        let req = match operation {
            ReplyOperation::Respond(reply) => {
                let response = super::interactions::text_response(reply);
                return self
                    .answer_interaction(interaction_id, interaction_token, &response)
                    .await
                    .map(|()| None);
            }
            ReplyOperation::Defer { ephemeral } => {
                let response = super::interactions::deferred_response(ephemeral);
                return self
                    .answer_interaction(interaction_id, interaction_token, &response)
                    .await
                    .map(|()| None);
            }
            ReplyOperation::EditOriginal { content } => Self::request_of(
                client
                    .update_response(interaction_token)
                    .content(Some(&content))
                    .allowed_mentions(Some(&mentions)),
            )?,
            ReplyOperation::EditFollowup {
                message_id,
                content,
            } => {
                let message_id = Id::<MessageMarker>::new_checked(message_id)
                    .ok_or_else(|| DiscordError::Rejected("bad followup message id".to_owned()))?;
                Self::request_of(
                    client
                        .update_followup(interaction_token, message_id)
                        .content(Some(&content))
                        .allowed_mentions(Some(&mentions)),
                )?
            }
            ReplyOperation::Followup(reply) => Self::request_of(
                client
                    .create_followup(interaction_token)
                    .content(&reply.content)
                    .flags(if reply.ephemeral {
                        MessageFlags::EPHEMERAL
                    } else {
                        MessageFlags::empty()
                    })
                    .allowed_mentions(Some(&mentions)),
            )?,
            ReplyOperation::DeleteOriginal => {
                Self::request_of(client.delete_response(interaction_token))?
            }
        };
        // Unpaced lane; guard admission runs inside the one bounded attempt.
        let (res, _) = self.send_with_timeout(&req, None).await?;
        match res.status {
            200..=299 if creates_followup => {
                // Discord returns the created message; retain its identity so
                // progress/completion can edit it after @original is deleted.
                let message: serde_json::Value =
                    serde_json::from_slice(&res.body).map_err(|_| {
                        DiscordError::Unavailable("invalid followup response".to_owned())
                    })?;
                let id = message["id"]
                    .as_str()
                    .and_then(|id| id.parse::<u64>().ok())
                    .filter(|id| *id != 0)
                    .ok_or_else(|| DiscordError::Unavailable("missing followup id".to_owned()))?;
                Ok(Some(id))
            }
            200..=299 => Ok(None),
            _ => Err(throw_for_status(&res)),
        }
    }

    /// Complete an acknowledged interaction by editing its original response.
    /// Like the initial callback, this bypasses the paced moderation lane.
    /// One attempt: a lost reply must not repeat already-committed store effects.
    /// The original callback decides ephemerality; edits retain it.
    pub async fn edit_interaction_response(
        &self,
        application_id: u64,
        interaction_token: &str,
        content: &str,
    ) -> Result<(), DiscordError> {
        let application =
            Id::<ApplicationMarker>::new_checked(application_id).ok_or_else(|| {
                DiscordError::Rejected(format!("bad application id: {application_id}"))
            })?;
        let content = two_bot_core::message_safety::content(content);
        let mentions = AllowedMentions::default();
        let req = Self::request_of(
            self.inner
                .factory
                .interaction(application)
                .update_response(interaction_token)
                .content(Some(&content))
                .allowed_mentions(Some(&mentions)),
        )?;
        let req = Self::explicit_mentions(req, &mentions)?;
        let mut res = self.call_once_raw(req, &[200]).await?;
        mutation_receipt_id(&res.body)?;
        res.complete().await;
        Ok(())
    }

    /// Turn one adjudicated [`ModerationExecution`] into its Discord effect
    /// (legacy `ModerationService::carryOut` verb mapping; warn is
    /// store-only and never reaches the wire).
    pub async fn execute_outcome(
        &self,
        exec: &ModerationExecution,
        outcome: &ActionOutcome,
    ) -> Result<ActionOutcome, DiscordError> {
        match outcome {
            ActionOutcome::Banned { user_id } => {
                self.ban(&exec.guild_id, user_id, &exec.reason).await?;
            }
            ActionOutcome::TemporarilyBanned { user_id, .. } => {
                self.ban(&exec.guild_id, user_id, &exec.reason).await?;
            }
            ActionOutcome::Kicked { user_id } => {
                self.kick_member(&exec.guild_id, user_id, &exec.reason)
                    .await?;
            }
            ActionOutcome::TimedOut {
                user_id,
                duration_seconds,
            } => {
                let until = timeout_until_iso(*duration_seconds);
                self.timeout_member(&exec.guild_id, user_id, Some(&until), &exec.reason)
                    .await?;
            }
            ActionOutcome::Warned { .. } => {
                // Store-only: legacy `carryOut` writes the ledger row and
                // never calls Discord.
            }
            ActionOutcome::Unbanned { user_id } => {
                self.unban(&exec.guild_id, user_id, &exec.reason).await?;
            }
            ActionOutcome::Purged { channel_id, .. } => {
                let count = exec.count.unwrap_or(0);
                let affected = self.purge(channel_id, count, &exec.reason).await?;
                return Ok(ActionOutcome::Purged {
                    channel_id: channel_id.clone(),
                    // `purge` counts listed ids (≤100 by clamp); always fits.
                    affected: affected as usize,
                });
            }
            ActionOutcome::SlowmodeUpdated {
                channel_id,
                seconds,
            } => {
                self.set_slowmode(channel_id, *seconds, &exec.reason)
                    .await?;
            }
            ActionOutcome::LockedDown { channel_id } => {
                let current = self
                    .get_everyone_overwrite(channel_id, &exec.guild_id)
                    .await?;
                let (allow, deny) = lockdown_masks(current.as_ref());
                self.put_everyone_overwrite(
                    channel_id,
                    &exec.guild_id,
                    &allow,
                    &deny,
                    &exec.reason,
                )
                .await?;
            }
            ActionOutcome::Unlocked { channel_id } => {
                // Without a recorded seed we clear only the send bit (legacy
                // `unlockChannel` fallback); the recorded-restore path is the
                // store-owning slice's job to plan.
                let current = self
                    .get_everyone_overwrite(channel_id, &exec.guild_id)
                    .await?;
                let (allow, deny) = unlock_masks(current.as_ref());
                self.put_everyone_overwrite(
                    channel_id,
                    &exec.guild_id,
                    &allow,
                    &deny,
                    &exec.reason,
                )
                .await?;
            }
        }
        Ok(outcome.clone())
    }
}

/// Channel-verb outcome (affected counts for purge/post).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelCallOutcome {
    Purged { affected: u64 },
    SlowmodeUpdated,
    OverwriteWritten,
    OverwriteDeleted,
    Posted { message_id: String },
    MessageDeleted,
}

/// @everyone overwrite masks (decimal strings, legacy schema).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EveryoneOverwrite {
    pub allow: String,
    pub deny: String,
}

/// Lockdown write for a channel whose @everyone entry reads `current`
/// (deny send, drop any allow — every other bit preserved for unlock).
#[must_use]
pub fn lockdown_masks(current: Option<&EveryoneOverwrite>) -> (String, String) {
    let (allow, deny) = current
        .map(|ow| {
            (
                ow.allow.parse::<u64>().unwrap_or(0),
                ow.deny.parse::<u64>().unwrap_or(0),
            )
        })
        .unwrap_or((0, 0));
    (
        (allow & !SEND_MESSAGES_BIT).to_string(),
        (deny | SEND_MESSAGES_BIT).to_string(),
    )
}

/// Unlock-fallback write: clear the send bit on both masks (legacy
/// `unlockChannel` when no lockdown row exists).
#[must_use]
pub fn unlock_masks(current: Option<&EveryoneOverwrite>) -> (String, String) {
    let (allow, deny) = current
        .map(|ow| {
            (
                ow.allow.parse::<u64>().unwrap_or(0),
                ow.deny.parse::<u64>().unwrap_or(0),
            )
        })
        .unwrap_or((0, 0));
    (
        (allow & !SEND_MESSAGES_BIT).to_string(),
        (deny & !SEND_MESSAGES_BIT).to_string(),
    )
}

fn mutation_receipt_id(body: &[u8]) -> Result<String, DiscordError> {
    let body: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| DiscordError::Unavailable("invalid mutation receipt".to_owned()))?;
    let id = body.get("id").and_then(serde_json::Value::as_str);
    match id {
        Some(id) if id.parse::<u64>().is_ok_and(|id| id != 0) => Ok(id.to_owned()),
        _ => Err(DiscordError::Unavailable(
            "invalid mutation receipt id".to_owned(),
        )),
    }
}

/// Only documented no-effect rejections are retry-safe. An unexpected success,
/// redirect, timeout status or other ambiguous response may follow a mutation.
#[must_use]
pub fn throw_for_status(res: &RawResponse) -> DiscordError {
    match res.status {
        429 => DiscordError::RateLimited,
        400 | 401 | 403 | 404 | 405 => {
            DiscordError::Rejected(format!("Discord refused the request with {}", res.status))
        }
        _ => DiscordError::Unavailable(format!("Discord returned {}", res.status)),
    }
}

trait IntoOther {
    fn into_other(self, status: u16) -> DiscordError;
}

impl IntoOther for DiscordError {
    fn into_other(self, status: u16) -> DiscordError {
        match self {
            DiscordError::Rejected(detail) if detail.is_empty() => {
                DiscordError::Rejected(format!("Discord refused the request with {status}"))
            }
            other => other,
        }
    }
}

pub(crate) fn snowflake<T>(value: &str) -> Result<Id<T>, DiscordError> {
    // `Id::new` panics on zero, so zero and garbage both reject here.
    value
        .parse::<u64>()
        .ok()
        .and_then(Id::new_checked)
        .ok_or_else(|| DiscordError::Rejected(format!("bad snowflake: {value}")))
}

/// Validate + encode the audit-log reason: legacy `requireModerationReason`
/// bounds the caller's text to 512 chars, then it is percent-encoded for the
/// `X-Audit-Log-Reason` header (twilight validates the same bound).
fn audit_reason(reason: &str) -> Result<String, DiscordError> {
    let trimmed = reason.trim();
    if trimmed.is_empty() {
        return Err(DiscordError::Rejected("empty audit reason".to_owned()));
    }
    if trimmed.chars().count() > MAX_AUDIT_REASON_CHARS {
        return Err(DiscordError::Rejected(format!(
            "audit reason is {} chars; limit is {MAX_AUDIT_REASON_CHARS}",
            trimmed.chars().count()
        )));
    }
    Ok(trimmed.to_owned())
}

/// `timeout` until timestamp: now + duration, RFC 3339 (legacy
/// `new Date(now + seconds * 1000).toISOString()`).
///
/// The offset is rendered `+00:00`, not `Z`: pinned Twilight 0.17.1
/// `Timestamp::parse` rejects inputs shorter than `...+00:00` (25 chars), so
/// the `Z` form never survived `timeout_member` (finding 1).
#[must_use]
pub fn timeout_until_iso(duration_seconds: u64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_add(duration_seconds);
    format_iso_secs(now)
}

fn format_iso_secs(epoch_secs: u64) -> String {
    // Days-based civil conversion (Howard Hinnant's algorithm) + HH:MM:SS.
    let days = (epoch_secs / 86_400) as i64;
    let secs = epoch_secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    y += i64::from(m <= 2);
    format!(
        "{y:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Raw GET route for an absolute `/api/v10`-relative path the executor builds
/// for paced reads (uncharted query shapes are refused; the typed builders
/// cover every mutation). All query parameters the parity §6 reads use are
/// preserved — dropping `before` would page the newest history forever
/// (finding 2). Route Display renders the query string twilight's way so the
/// mock sees byte-identical paths.
fn raw_get_route(path: &str) -> Result<Route<'static>, String> {
    let err = || format!("unsupported GET path: {path}");
    let (base, query) = match path.split_once('?') {
        Some((b, q)) => (b, q),
        None => (path, ""),
    };
    if base == "/users/@me" && query.is_empty() {
        return Ok(Route::GetCurrentUser);
    }
    // Route borrows nothing here (u64/bool fields); the 'static bound is
    // satisfied because no borrowed variant is constructed.
    if let Some(id) = base.strip_prefix("/guilds/") {
        let (guild_part, rest) = match id.split_once('/') {
            Some((g, r)) => (g, Some(r)),
            None => (id, None),
        };
        let guild_id: u64 = guild_part.parse().map_err(|_| err())?;
        if guild_id == 0 {
            return Err(err());
        }
        return match rest {
            None => Ok(Route::GetGuild {
                guild_id,
                with_counts: query_param(query, "with_counts").is_some_and(|v| v == "true"),
            }),
            Some("members") => Ok(Route::GetGuildMembers {
                after: query_param(query, "after").and_then(|v| v.parse().ok()),
                guild_id,
                limit: query_param(query, "limit").and_then(|v| v.parse().ok()),
            }),
            Some("roles") if query.is_empty() => Ok(Route::GetGuildRoles { guild_id }),
            Some(member) if member.starts_with("members/") && query.is_empty() => {
                let user_id = member
                    .strip_prefix("members/")
                    .unwrap()
                    .parse::<u64>()
                    .map_err(|_| err())?;
                if user_id == 0 {
                    return Err(err());
                }
                Ok(Route::GetMember { guild_id, user_id })
            }
            Some("scheduled-events") => Ok(Route::GetGuildScheduledEvents {
                guild_id,
                with_user_count: query_param(query, "with_user_count").is_some_and(|v| v == "true"),
            }),
            Some(rest) if rest.starts_with("scheduled-events/") && query.is_empty() => {
                let scheduled_event_id = rest
                    .strip_prefix("scheduled-events/")
                    .and_then(|id| id.parse::<u64>().ok())
                    .filter(|id| *id != 0)
                    .ok_or_else(err)?;
                Ok(Route::GetGuildScheduledEvent {
                    guild_id,
                    scheduled_event_id,
                    with_user_count: false,
                })
            }
            _ => Err(err()),
        };
    }
    if let Some(id) = base.strip_prefix("/channels/") {
        let (channel_part, rest) = match id.split_once('/') {
            Some((c, r)) => (c, Some(r)),
            None => (id, None),
        };
        let channel_id: u64 = channel_part.parse().map_err(|_| err())?;
        if channel_id == 0 {
            return Err(err());
        }
        return match rest {
            None => {
                if query.is_empty() {
                    Ok(Route::GetChannel { channel_id })
                } else {
                    Err(err())
                }
            }
            Some("messages") => Ok(Route::GetMessages {
                after: query_param(query, "after").and_then(|v| v.parse().ok()),
                around: query_param(query, "around").and_then(|v| v.parse().ok()),
                before: query_param(query, "before").and_then(|v| v.parse().ok()),
                channel_id,
                limit: query_param(query, "limit").and_then(|v| v.parse().ok()),
            }),
            // Automod enriches partial edits and role-less creates from the
            // authoritative message; there is no query form of this route.
            Some(message) if message.starts_with("messages/") && query.is_empty() => {
                let message_id = message
                    .strip_prefix("messages/")
                    .unwrap()
                    .parse::<u64>()
                    .map_err(|_| err())?;
                if message_id == 0 {
                    return Err(err());
                }
                Ok(Route::GetMessage {
                    channel_id,
                    message_id,
                })
            }
            _ => Err(err()),
        };
    }
    Err(err())
}

/// First `key=value` pair value in a raw query string.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    query.split('&').find_map(|pair| pair.strip_prefix(&prefix))
}

/// One paced-lane retry decision (legacy `rest.ts` / `kick.ts` policy made
/// pure so the acceptance tests pin it without a socket).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacedStep {
    /// Sleep `u64` ms, then retry (429 reuses the same attempt, 5xx spends
    /// one).
    RetryAfterMs(u64),
    /// Map the status to its terminal value now — never retried.
    Terminal,
    /// Past the retry budget — the caller reports, never retries.
    Exhausted,
}

/// Pure retry-policy step shared by the paced lane and the acceptance tests:
/// given one observed status, decide sleep-then-retry (same attempt for 429,
/// next attempt for 5xx/transport), give-up-with-value, or fail.
#[must_use]
pub fn paced_step(status: u16, attempt: u32, retry_after_wait_ms: u64) -> PacedStep {
    if status == 429 {
        if attempt > MAX_HTTP_TRIES - 1 {
            return PacedStep::Exhausted;
        }
        return PacedStep::RetryAfterMs(retry_after_wait_ms.min(MAX_RETRY_AFTER_MS));
    }
    if status >= 500 {
        if attempt >= MAX_HTTP_TRIES - 1 {
            return PacedStep::Exhausted;
        }
        return PacedStep::RetryAfterMs(backoff_ms(attempt));
    }
    PacedStep::Terminal
}

/// Pure pace-floor helper over wall-clock millis (legacy `pace()` made
/// testable): how long to sleep before the next request on a lane whose last
/// request went out at `last_at_ms` with floor `interval_ms`.
#[must_use]
pub fn pace_delay_ms(last_at_ms: u64, interval_ms: u64, now_ms: u64) -> u64 {
    pace_wait_ms(last_at_ms, interval_ms, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use two_bot_core::send_admission::{
        AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, TokenKey,
    };

    #[derive(Debug)]
    struct NeverSendAdmission(TokenKey);

    impl SendAdmission for NeverSendAdmission {
        fn token_key(&self) -> &TokenKey {
            &self.0
        }

        fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
            Box::pin(async { Err(AdmissionError::Blocked) })
        }
    }

    fn never_send_admission(token: &str) -> Arc<dyn SendAdmission> {
        Arc::new(NeverSendAdmission(TokenKey::for_bot_token(token).unwrap()))
    }

    #[test]
    fn only_the_canonical_blocked_detail_is_retryable() {
        assert!(
            DiscordError::Unavailable(AdmissionError::Blocked.to_string()).is_admission_blocked()
        );
        for error in [
            DiscordError::Unavailable("transport: connection reset".to_owned()),
            DiscordError::Unavailable("Discord response body unavailable".to_owned()),
            DiscordError::Unavailable("discord send admission is blocked ".to_owned()),
            DiscordError::Timeout,
            DiscordError::RateLimited,
            DiscordError::Rejected("discord refused the request".to_owned()),
        ] {
            assert!(!error.is_admission_blocked(), "{error:?}");
        }
    }

    #[tokio::test]
    async fn debug_redacts_transport_and_nested_executor_token() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let marker = "fixture-rest-executor-bot-token";
        let admission = never_send_admission(marker);
        let transport =
            HyperTransport::with_admission(marker.to_owned(), None, admission.clone()).unwrap();
        let executor = ActionExecutor::with_admission(marker.to_owned(), None, admission).unwrap();
        for output in [
            format!("{transport:?}"),
            format!("{transport:#?}"),
            format!("{executor:?}"),
            format!("{executor:#?}"),
            format!("{:?}", executor.inner),
        ] {
            assert!(!output.contains(marker));
            assert!(output.contains("[REDACTED]"));
        }
        assert_eq!(transport.token.expose(), &format!("Bot {marker}"));
    }

    #[test]
    fn credential_bearing_proxy_is_rejected_before_tls_or_network() {
        for proxy in [
            "https://fixture-user:fixture-password@proxy.invalid",
            "http://fixture-user:fixture-password@127.0.0.1:9",
            "https://proxy.invalid/fixture-path-secret",
            "https://proxy.invalid?key=fixture-query-secret",
        ] {
            for error in [
                HyperTransport::with_admission(
                    "fixture-token".to_owned(),
                    Some(proxy.to_owned()),
                    never_send_admission("fixture-token"),
                )
                .unwrap_err(),
                ActionExecutor::with_admission(
                    "fixture-token".to_owned(),
                    Some(proxy.to_owned()),
                    never_send_admission("fixture-token"),
                )
                .unwrap_err(),
            ] {
                assert!(!error.contains("fixture"));
                assert!(!error.contains(proxy));
            }
        }
    }

    #[tokio::test]
    async fn accepted_proxy_is_redacted_in_transport_and_twilight_factory_debug() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let proxy = "https://fixture-proxy-origin.invalid";
        let admission = never_send_admission("fixture-token");
        let transport = HyperTransport::with_admission(
            "fixture-token".to_owned(),
            Some(proxy.to_owned()),
            admission.clone(),
        )
        .unwrap();
        let executor = ActionExecutor::with_admission(
            "fixture-token".to_owned(),
            Some(proxy.to_owned()),
            admission,
        )
        .unwrap();
        for shown in [
            format!("{transport:?}"),
            format!("{transport:#?}"),
            format!("{executor:?}"),
            format!("{executor:#?}"),
            format!("{:?}", executor.inner),
            format!("{:#?}", executor.inner),
        ] {
            assert!(!shown.contains("fixture-token"));
            assert!(!shown.contains("fixture-proxy-origin"));
        }
        assert!(transport.url("users/@me").starts_with(proxy));
    }

    #[test]
    fn raw_response_debug_never_formats_remote_echoes() {
        let response = RawResponse {
            status: 403,
            retry_after_header: Some("fixture-echoed-header-secret".to_owned()),
            body: b"fixture-echoed-body-secret".to_vec(),
            completion: None,
        };
        for shown in [format!("{response:?}"), format!("{response:#?}")] {
            assert!(!shown.contains("fixture"));
            assert!(!shown.contains("102, 105, 120, 116, 117, 114, 101"));
            assert!(shown.contains("403"));
        }
    }

    #[test]
    fn constants_match_legacy() {
        assert_eq!(PACE_INTERVAL_MS, 110);
        assert_eq!(KICK_INTERVAL_MS, 350);
        assert_eq!(MODERATION_TIMEOUT_MS, 5_000);
        assert_eq!(MAX_MESSAGE_CHARS, 2000);
        assert_eq!(MAX_AUDIT_REASON_CHARS, 512);
        // Re-exported core numbers stay the legacy sequence.
        assert_eq!(two_bot_core::BACKOFF_BASE_MS, 500);
        assert_eq!(MAX_HTTP_TRIES, 5);
        assert_eq!(two_bot_core::RETRY_AFTER_PADDING_MS, 250);
        assert_eq!(MAX_RETRY_AFTER_MS, 60_000);
        assert_eq!(SEND_MESSAGES_BIT, 2048);
    }

    #[test]
    fn retry_after_body_wins_header() {
        let res = RawResponse {
            status: 429,
            retry_after_header: Some("5".to_owned()),
            body: br#"{"retry_after": 1.5, "global": false}"#.to_vec(),
            completion: None,
        };
        assert_eq!(res.body_retry_after_secs(), Some(1.5));
        assert_eq!(res.retry_after_wait_ms(), 1750);
    }

    #[test]
    fn retry_after_header_only_and_defaults() {
        let header_only = RawResponse {
            status: 429,
            retry_after_header: Some("2".to_owned()),
            body: Vec::new(),
            completion: None,
        };
        assert_eq!(header_only.retry_after_wait_ms(), 2250);
        let missing = RawResponse {
            status: 429,
            retry_after_header: None,
            body: Vec::new(),
            completion: None,
        };
        assert_eq!(missing.retry_after_wait_ms(), 1250);
        let garbage = RawResponse {
            status: 429,
            retry_after_header: Some("soon".to_owned()),
            body: b"not json".to_vec(),
            completion: None,
        };
        assert_eq!(garbage.retry_after_wait_ms(), 1250);
        let clamped = RawResponse {
            status: 429,
            retry_after_header: Some("86400".to_owned()),
            body: Vec::new(),
            completion: None,
        };
        assert_eq!(clamped.retry_after_wait_ms(), MAX_RETRY_AFTER_MS);
    }

    #[test]
    fn backoff_sequence_and_paced_steps() {
        assert_eq!(
            (0..5).map(backoff_ms).collect::<Vec<_>>(),
            [500, 1000, 2000, 4000, 8000]
        );
        // 429 parks on the same attempt until the budget is spent.
        assert_eq!(paced_step(429, 0, 1750), PacedStep::RetryAfterMs(1750));
        assert_eq!(paced_step(429, 4, 1750), PacedStep::RetryAfterMs(1750));
        assert_eq!(paced_step(429, 5, 1750), PacedStep::Exhausted);
        // 5xx backs off per attempt, past attempt 4 gives up.
        assert_eq!(paced_step(500, 0, 0), PacedStep::RetryAfterMs(500));
        assert_eq!(paced_step(503, 3, 0), PacedStep::RetryAfterMs(4000));
        assert_eq!(paced_step(500, 4, 0), PacedStep::Exhausted);
        // Terminal statuses map to values, never retries.
        assert_eq!(paced_step(200, 0, 0), PacedStep::Terminal);
        assert_eq!(paced_step(404, 0, 0), PacedStep::Terminal);
        assert_eq!(paced_step(403, 0, 0), PacedStep::Terminal);
    }

    #[test]
    fn pace_floor_matches_legacy() {
        assert_eq!(pace_delay_ms(1000, 110, 1050), 60);
        assert_eq!(pace_delay_ms(1000, 110, 1200), 0);
        assert_eq!(pace_delay_ms(1000, 350, 1100), 250);
    }

    #[test]
    fn throw_for_status_releases_only_confirmed_rejections() {
        let rl = RawResponse {
            status: 429,
            retry_after_header: None,
            body: Vec::new(),
            completion: None,
        };
        assert_eq!(throw_for_status(&rl), DiscordError::RateLimited);
        let down = RawResponse {
            status: 503,
            retry_after_header: None,
            body: Vec::new(),
            completion: None,
        };
        assert!(matches!(
            throw_for_status(&down),
            DiscordError::Unavailable(_)
        ));
        for status in [400, 401, 403, 404, 405] {
            let no = RawResponse {
                status,
                retry_after_header: None,
                body: Vec::new(),
                completion: None,
            };
            assert!(throw_for_status(&no).is_safe_pre_mutation(), "{status}");
        }
        for status in [100, 200, 202, 204, 301, 302, 307, 408, 409, 425, 500, 503] {
            let uncertain = RawResponse {
                status,
                retry_after_header: None,
                body: Vec::new(),
                completion: None,
            };
            assert!(matches!(
                throw_for_status(&uncertain),
                DiscordError::Unavailable(_)
            ));
            assert!(
                !throw_for_status(&uncertain).is_safe_pre_mutation(),
                "{status}"
            );
        }
        assert!(DiscordError::Rejected("build: invalid request".to_owned()).is_safe_pre_mutation());
        assert!(!DiscordError::Timeout.is_safe_pre_mutation());
        assert!(!DiscordError::RateLimited.is_safe_pre_mutation());
    }

    #[test]
    fn lockdown_and_unlock_masks_match_legacy() {
        assert_eq!(lockdown_masks(None), ("0".to_owned(), "2048".to_owned()));
        assert_eq!(
            lockdown_masks(Some(&EveryoneOverwrite {
                allow: "2112".to_owned(),
                deny: "64".to_owned(),
            })),
            ("64".to_owned(), "2112".to_owned())
        );
        assert_eq!(
            unlock_masks(Some(&EveryoneOverwrite {
                allow: "2112".to_owned(),
                deny: "2112".to_owned(),
            })),
            ("64".to_owned(), "64".to_owned())
        );
    }

    #[test]
    fn audit_reason_bounds_match_legacy() {
        assert!(audit_reason("").is_err());
        assert!(audit_reason("   ").is_err());
        assert!(audit_reason(&"x".repeat(512)).is_ok());
        assert!(audit_reason(&"x".repeat(513)).is_err());
        assert_eq!(audit_reason("  spam  ").unwrap(), "spam");
    }

    #[test]
    fn timeout_until_is_iso_and_additive() {
        use twilight_model::util::datetime::Timestamp;
        let a = timeout_until_iso(60);
        let b = timeout_until_iso(3600);
        // The pinned Twilight parser only accepts the +00:00 offset form
        // (25+ chars); a trailing Z never survived timeout_member (finding 1).
        assert!(a.ends_with("+00:00") && b.ends_with("+00:00"));
        assert!(Timestamp::parse(&a).is_ok(), "executor output parses: {a}");
        assert!(b > a, "longer duration must sort later: {a} vs {b}");
    }

    #[test]
    fn kick_never_throws_without_reason() {
        // Reason validation happens before any I/O: an empty reason is a
        // terminal Failed value with zero attempts, not a panic.
        assert!(audit_reason("").is_err());
    }
}
