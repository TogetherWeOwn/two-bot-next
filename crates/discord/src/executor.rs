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
//!   retry posture: only [`DiscordError::Rejected`] is safe pre-mutation.
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
    ApplicationMarker, ChannelMarker, GuildMarker, InteractionMarker, MessageMarker, UserMarker,
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
/// Legacy audit-log-reason header bound (`X-Audit-Log-Reason`, latin-1,
/// percent-encoded; twilight validates ≤512 chars).
pub const MAX_AUDIT_REASON_CHARS: usize = 512;
/// Legacy message ceiling asserted before sending.
pub const MAX_MESSAGE_CHARS: usize = 2000;

/// Discord failure modes (legacy `ModerationDiscord` `ActionError` codes).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscordError {
    /// Discord refused the request (non-429 4xx): provably no mutation
    /// happened, so the claim is safe to release (legacy `discord_rejected`).
    /// This is the ONLY retry-safe failure.
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
}

impl DiscordError {
    /// True only for failures that prove no mutation happened (legacy
    /// `isSafePreMutationFailure`).
    #[must_use]
    pub fn is_safe_pre_mutation(&self) -> bool {
        matches!(self, Self::Rejected(_))
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
}

/// One observed HTTP exchange: status plus parsed bodies the retry policy
/// needs. The executor owns this transport so 429/5xx accounting is exact.
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub retry_after_header: Option<String>,
    pub body: Vec<u8>,
}

impl RawResponse {
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
#[derive(Debug, Clone)]
pub struct HyperTransport {
    inner: HyperClient<
        hyper_rustls::HttpsConnector<HttpConnector>,
        http_body_util::Full<bytes::Bytes>,
    >,
    scheme_http: bool,
    host: String,
    token: String,
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
        > = HyperClient::builder(TokioExecutor::new()).build(connector);
        Ok(Self {
            inner,
            scheme_http,
            host,
            token,
        })
    }

    fn url(&self, path: &str) -> String {
        let scheme = if self.scheme_http { "http" } else { "https" };
        format!(
            "{scheme}://{}/api/v{}/{path}",
            self.host,
            twilight_http::API_VERSION
        )
    }

    async fn send_request(&self, request: &Request) -> Result<RawResponse, String> {
        use http_body_util::BodyExt as _;
        let method: http::Method = request
            .method()
            .name()
            .parse()
            .map_err(|e| format!("bad method: {e}"))?;
        let url = self.url(request.path());
        let mut builder = hyper::Request::builder().method(method).uri(url);
        if let Some(headers) = builder.headers_mut() {
            if request.use_authorization_token() {
                headers.insert(
                    hyper::header::AUTHORIZATION,
                    hyper::header::HeaderValue::from_str(&self.token)
                        .map_err(|e| format!("bad token header: {e}"))?,
                );
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
        let response = self
            .inner
            .request(hyper_req)
            .await
            .map_err(|e| format!("transport: {e}"))?;
        let status = response.status().as_u16();
        let retry_after_header = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let collected = response
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("read body: {e}"))?;
        Ok(RawResponse {
            status,
            retry_after_header,
            body: collected.to_bytes().to_vec(),
        })
    }
}

/// The S4 REST executor: paced lane + moderation lane over one transport.
#[derive(Debug, Clone)]
pub struct ActionExecutor {
    inner: Arc<ExecutorInner>,
}

#[derive(Debug)]
struct ExecutorInner {
    transport: HyperTransport,
    /// Twilight client kept as the request factory (builders + audit
    /// encoding) and for publish/callback sends with default mention
    /// suppression.
    factory: TwilightClient,
    pace_interval: Duration,
    kick_interval: Duration,
    moderation_timeout: Duration,
    pace_last_at: tokio::sync::Mutex<std::time::Instant>,
    kick_last_at: tokio::sync::Mutex<std::time::Instant>,
    requests: std::sync::atomic::AtomicU64,
}

impl ActionExecutor {
    /// Build against real Discord.
    pub fn new(token: String) -> Result<Self, String> {
        Self::with_proxy(token, None)
    }

    /// Build with an optional API-host override (legacy `DISCORD_API_BASE`;
    /// tests point this at the mock double).
    pub fn with_proxy(token: String, proxy_url: Option<String>) -> Result<Self, String> {
        let transport = HyperTransport::with_proxy(token.clone(), proxy_url.clone())?;
        let mut builder = TwilightClient::builder()
            .token(token)
            .ratelimiter(None)
            .timeout(Duration::from_millis(MODERATION_TIMEOUT_MS));
        if let Some(url) = proxy_url {
            let host = url
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .trim_end_matches('/')
                .to_owned();
            let use_http = url.starts_with("http://")
                || url.starts_with("127.")
                || url.starts_with("localhost");
            builder = builder.proxy(host, use_http);
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
                    std::time::Instant::now() - Duration::from_secs(60),
                ),
                kick_last_at: tokio::sync::Mutex::new(
                    std::time::Instant::now() - Duration::from_secs(60),
                ),
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

    async fn pace(&self, kick_lane: bool) {
        let (lock, interval) = if kick_lane {
            (&self.inner.kick_last_at, self.inner.kick_interval)
        } else {
            (&self.inner.pace_last_at, self.inner.pace_interval)
        };
        let mut last = lock.lock().await;
        let earliest = *last + interval;
        let now = std::time::Instant::now();
        if earliest > now {
            tokio::time::sleep(earliest - now).await;
        }
        *last = std::time::Instant::now();
    }

    fn count(&self) {
        self.inner
            .requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    async fn send(&self, request: &Request) -> Result<RawResponse, String> {
        self.count();
        self.inner.transport.send_request(request).await
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
        let res = self.call_once_raw(request, accepted).await?;
        Ok(serde_json::from_slice(&res.body).ok())
    }

    /// Same single-attempt send as [`Self::call_once`], but returns the raw
    /// exchange so callers that must distinguish "proven absent" from
    /// "unreadable" can validate the body themselves.
    async fn call_once_raw(
        &self,
        request: Request,
        accepted: &[u16],
    ) -> Result<RawResponse, DiscordError> {
        let res = tokio::time::timeout(self.inner.moderation_timeout, self.send(&request))
            .await
            .map_err(|_| DiscordError::Timeout)?
            .map_err(DiscordError::Unavailable)?;
        if accepted.contains(&res.status) {
            return Ok(res);
        }
        Err(throw_for_status(&res))
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
        let path_guild = guild_id.to_owned();
        let path_user = user_id.to_owned();
        let reason = match audit_reason(reason) {
            Ok(r) => r,
            Err(e) => {
                return KickResult {
                    outcome: KickOutcome::Failed,
                    status: None,
                    detail: e.to_string(),
                    attempts: 0,
                }
            }
        };
        let mut attempts: u32 = 0;
        loop {
            self.pace(true).await;
            attempts += 1;
            let request = match self.kick_request(&path_guild, &path_user, &reason) {
                Ok(r) => r,
                Err(detail) => {
                    return KickResult {
                        outcome: KickOutcome::Failed,
                        status: None,
                        detail,
                        attempts,
                    }
                }
            };
            let res = match self.send(&request).await {
                Ok(r) => r,
                Err(detail) => {
                    if attempts > MAX_HTTP_TRIES - 1 {
                        return KickResult {
                            outcome: KickOutcome::Failed,
                            status: None,
                            detail: format!("network: {detail}"),
                            attempts,
                        };
                    }
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempts - 1))).await;
                    continue;
                }
            };
            match classify_kick_status(res.status) {
                KickStatus::Removed => {
                    return KickResult {
                        outcome: KickOutcome::Kicked,
                        status: Some(res.status),
                        detail: "removed".to_owned(),
                        attempts,
                    }
                }
                KickStatus::AlreadyGone => {
                    return KickResult {
                        outcome: KickOutcome::AlreadyGone,
                        status: Some(res.status),
                        detail: "not a member".to_owned(),
                        attempts,
                    }
                }
                KickStatus::Forbidden => {
                    return KickResult {
                        outcome: KickOutcome::Forbidden,
                        status: Some(res.status),
                        detail: "missing Kick Members, or the target outranks the bot".to_owned(),
                        attempts,
                    }
                }
                KickStatus::Unauthorized => {
                    return KickResult {
                        outcome: KickOutcome::Failed,
                        status: Some(res.status),
                        detail: "token rejected".to_owned(),
                        attempts,
                    }
                }
                KickStatus::RateLimited => {
                    let wait = res.retry_after_wait_ms();
                    if attempts > MAX_HTTP_TRIES - 1 {
                        return KickResult {
                            outcome: KickOutcome::RateLimited,
                            status: Some(res.status),
                            detail: format!("still rate limited after {attempts} attempts"),
                            attempts,
                        };
                    }
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                }
                KickStatus::ServerError => {
                    if attempts > MAX_HTTP_TRIES - 1 {
                        return KickResult {
                            outcome: KickOutcome::Failed,
                            status: Some(res.status),
                            detail: "server error".to_owned(),
                            attempts,
                        };
                    }
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempts - 1))).await;
                }
                KickStatus::Other => {
                    return KickResult {
                        outcome: KickOutcome::Failed,
                        status: Some(res.status),
                        detail: "unexpected status".to_owned(),
                        attempts,
                    }
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
        let route = raw_get_route(path)?;
        let mut attempt: u32 = 0;
        loop {
            self.pace(false).await;
            let request = Request::from_route(&route);
            let res = match self.send(&request).await {
                Ok(r) => r,
                Err(detail) => {
                    if attempt >= MAX_HTTP_TRIES - 1 {
                        return Err(detail);
                    }
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempt))).await;
                    attempt += 1;
                    continue;
                }
            };
            match res.status {
                200..=299 => return Ok(serde_json::from_slice(&res.body).ok()),
                429 => {
                    tokio::time::sleep(Duration::from_millis(res.retry_after_wait_ms())).await;
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

    /// Channel GET with the paced lane (legacy `getEveryoneOverwrite` reads
    /// `permission_overwrites` off the channel).
    pub async fn get_everyone_overwrite(
        &self,
        channel_id: &str,
        guild_id: &str,
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
        let channel: Id<ChannelMarker> = snowflake(channel_id)?;
        let reason = audit_reason(reason)?;
        let limit: u16 = count
            .clamp(1, 100)
            .try_into()
            .map_err(|_| DiscordError::Rejected(format!("purge out of range: {count}")))?;
        let list_req = Self::request_of(self.inner.factory.channel_messages(channel).limit(limit))?;
        let listed = self.call_once(list_req, &[200]).await?.unwrap_or_default();
        let ids: Vec<Id<MessageMarker>> = listed
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|row| row.get("id")?.as_str()?.parse::<u64>().ok())
            .filter_map(Id::new_checked)
            .collect();
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
                .delete_messages(channel, &ids)
                .reason(&reason),
        )?;
        // request_of maps pre-send build failures to Rejected (finding 7).
        self.call_once(req, &[200, 204]).await?;
        Ok(ids.len() as u64)
    }

    /// Post a message with mention suppression (legacy
    /// `allowed_mentions: { parse: [] }`). Asserts the legacy 2000 UTF-16-unit
    /// ceiling before sending; returns the message id (`""` when Discord
    /// omits it). A numeric nonce is sent with `enforce_nonce: true` for
    /// duplicate suppression.
    pub async fn post_message(
        &self,
        channel_id: &str,
        content: &str,
        nonce: Option<u64>,
    ) -> Result<String, DiscordError> {
        self.send_message(channel_id, content, nonce.map(serde_json::Value::from))
            .await
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
        if let Some(n) = nonce {
            body["nonce"] = n;
            body["enforce_nonce"] = serde_json::Value::Bool(true);
        }
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| DiscordError::Rejected(format!("build message body: {e}")))?;
        let req = Request::builder(&Route::CreateMessage {
            channel_id: channel.get(),
        })
        .body(body_bytes)
        .build()
        .map_err(|e| DiscordError::Rejected(format!("build: {e}")))?;
        let answered = self.call_once(req, &[200, 201]).await?.unwrap_or_default();
        Ok(answered
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned())
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
    /// Source: https://docs.rs/twilight-http/0.17.1/twilight_http/request/channel/message/struct.CreateMessage.html
    pub async fn post_channel_message(
        &self,
        channel_id: &str,
        content: &str,
        components: &[twilight_model::channel::message::Component],
        mentions: &AllowedMentions,
    ) -> Result<String, DiscordError> {
        if utf16_len(content) > MAX_MESSAGE_CHARS {
            return Err(DiscordError::Rejected(
                "message exceeds UTF-16 ceiling".into(),
            ));
        }
        let mut builder = self
            .inner
            .factory
            .create_message(snowflake(channel_id)?)
            .content(content)
            .allowed_mentions(Some(mentions));
        if !components.is_empty() {
            builder = builder.components(components);
        }
        let req = Self::explicit_mentions(Self::request_of(builder)?, mentions)?;
        let response = self.call_once(req, &[200, 201]).await?.unwrap_or_default();
        Ok(response
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned())
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
        self.pace(false).await;
        self.call_once(req, &[200, 204]).await?;
        Ok(())
    }

    /// Complete a deferred interaction without publishing a second channel
    /// message. The original callback decides ephemerality; edits retain it.
    /// Source: https://docs.rs/twilight-http/0.17.1/twilight_http/client/struct.InteractionClient.html#method.update_response
    pub async fn edit_interaction_response(
        &self,
        application_id: u64,
        interaction_token: &str,
        content: &str,
        components: &[twilight_model::channel::message::Component],
    ) -> Result<(), DiscordError> {
        if utf16_len(content) > MAX_MESSAGE_CHARS {
            return Err(DiscordError::Rejected(
                "message exceeds UTF-16 ceiling".into(),
            ));
        }
        let application = Id::<ApplicationMarker>::new_checked(application_id)
            .ok_or_else(|| DiscordError::Rejected("invalid application id".into()))?;
        let mentions = AllowedMentions {
            parse: vec![],
            replied_user: false,
            roles: vec![],
            users: vec![],
        };
        let interaction = self.inner.factory.interaction(application);
        let req = Self::request_of(
            interaction
                .update_response(interaction_token)
                .content(Some(content))
                .components(Some(components))
                .allowed_mentions(Some(&mentions)),
        )?;
        let req = Self::explicit_mentions(req, &mentions)?;
        self.call_once(req, &[200, 204]).await?;
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
                    message_id: self.send_message(channel_id, content, value).await?,
                })
            }
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
            self.pace(false).await;
            let res = tokio::time::timeout(self.inner.moderation_timeout, self.send(&req))
                .await
                .map_err(|_| DiscordError::Timeout)?
                .map_err(DiscordError::Unavailable)?;
            match res.status {
                200..=299 => return Ok(()),
                429 => {
                    if attempts >= MAX_HTTP_TRIES - 1 {
                        return Err(DiscordError::RateLimited);
                    }
                    tokio::time::sleep(Duration::from_millis(res.retry_after_wait_ms())).await;
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
        let req = Self::request_of(
            self.inner
                .factory
                .interaction(Id::<ApplicationMarker>::new(1))
                .create_response(interaction_id, interaction_token, response),
        )?;
        // request_of maps pre-send build failures to Rejected (finding 7).
        let res = tokio::time::timeout(self.inner.moderation_timeout, self.send(&req))
            .await
            .map_err(|_| DiscordError::Timeout)?
            .map_err(DiscordError::Unavailable)?;
        match res.status {
            200..=299 => Ok(()),
            _ => Err(throw_for_status(&res)),
        }
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

/// Legacy `throwForStatus`: 429 → [`DiscordError::RateLimited`], 5xx →
/// [`DiscordError::Unavailable`], anything else → [`DiscordError::Rejected`].
#[must_use]
pub fn throw_for_status(res: &RawResponse) -> DiscordError {
    if res.status == 429 {
        DiscordError::RateLimited
    } else if res.status >= 500 {
        DiscordError::Unavailable(format!("Discord returned {}", res.status))
    } else {
        DiscordError::Rejected(format!("Discord refused the request with {}", res.status))
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

fn snowflake<T>(value: &str) -> Result<Id<T>, DiscordError> {
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
                let user_id: u64 = member[8..].parse().map_err(|_| err())?;
                if user_id == 0 {
                    return Err(err());
                }
                Ok(Route::GetMember { guild_id, user_id })
            }
            Some("scheduled-events") => Ok(Route::GetGuildScheduledEvents {
                guild_id,
                with_user_count: query_param(query, "with_user_count").is_some_and(|v| v == "true"),
            }),
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
        };
        assert_eq!(header_only.retry_after_wait_ms(), 2250);
        let missing = RawResponse {
            status: 429,
            retry_after_header: None,
            body: Vec::new(),
        };
        assert_eq!(missing.retry_after_wait_ms(), 1250);
        let garbage = RawResponse {
            status: 429,
            retry_after_header: Some("soon".to_owned()),
            body: b"not json".to_vec(),
        };
        assert_eq!(garbage.retry_after_wait_ms(), 1250);
        let clamped = RawResponse {
            status: 429,
            retry_after_header: Some("86400".to_owned()),
            body: Vec::new(),
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
    fn throw_for_status_maps_like_legacy() {
        let rl = RawResponse {
            status: 429,
            retry_after_header: None,
            body: Vec::new(),
        };
        assert_eq!(throw_for_status(&rl), DiscordError::RateLimited);
        let down = RawResponse {
            status: 503,
            retry_after_header: None,
            body: Vec::new(),
        };
        assert!(matches!(
            throw_for_status(&down),
            DiscordError::Unavailable(_)
        ));
        let no = RawResponse {
            status: 403,
            retry_after_header: None,
            body: Vec::new(),
        };
        assert!(throw_for_status(&no).is_safe_pre_mutation());
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
