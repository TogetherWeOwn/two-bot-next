//! Outbound Twitch Helix client: app-token cache plus subscription reconcile.
//!
//! The pure decision core lives in [`crate::twitch_eventsub`] (webhook
//! verification, `CallbackUrl`, `create_subscription_body`,
//! `parse_subscriptions`, `reconcile`). This module adds the parts that core
//! deliberately left out:
//!
//! - [`HelixCredentials`]: the Operator-provisioned Twitch client id/secret,
//!   read from the environment and wrapped in [`Secret`] so they can never
//!   be logged.
//! - [`AppToken`]: the cached client-credentials app token with
//!   refresh-before-expiry ([`TOKEN_REFRESH_SKEW_MS`]) and
//!   [`parse_token_response`].
//! - [`plan_boot_actions`]: the accumulated subscription list plus desired
//!   broadcasters in, deletes-before-creates plan out. Pure: runs at boot and
//!   after a `revocation`, never on a timer.
//! - [`HelixClient`]: executes the plan over a [`HelixTransport`]. Tests
//!   inject a scripted transport; production uses [`HyperHelixTransport`]
//!   (hyper 1.x over rustls, one fresh client per request, fixed hosts only).
//!
//! Threat model: the only remote hosts are the two Twitch constants below —
//! no caller-supplied URL ever reaches the socket layer, so there is no SSRF
//! surface. Secrets (client id/secret, app token, signing secret, request
//! bodies that embed them) travel only inside [`Secret`] and are exposed
//! solely at the header/body boundary. Error messages name environment
//! variables and HTTP statuses, never secret values.

use std::collections::HashSet;
use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::header::USER_AGENT;
use http::{Method, StatusCode};
use http_body_util::{BodyStream, Full};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use thiserror::Error;
use url::form_urlencoded;

use crate::twitch_eventsub::{
    create_subscription_body, delete_subscription_url, parse_subscriptions, reconcile, CallbackUrl,
    EventSubError, ExistingSubscription, SubscriptionAction, WebhookSecret,
    HELIX_SUBSCRIPTIONS_URL,
};
use crate::Secret;

/// Client-credentials token endpoint (fixed host; never caller-supplied).
pub const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
/// Refresh the cached app token this far ahead of its `expires_in` instant.
pub const TOKEN_REFRESH_SKEW_MS: i64 = 60_000;
/// One Helix/token HTTP round trip may take this long, DNS to last byte.
pub const HELIX_TIMEOUT_MS: u64 = 15_000;
/// Largest response body the client buffers; subscription listings are small.
pub const HELIX_MAX_BODY_BYTES: usize = 64 * 1024;
/// Maximum number of pages accepted before reconciliation fails closed.
pub const HELIX_MAX_SUBSCRIPTION_PAGES: usize = 100;
/// User-Agent for the token and Helix calls.
pub const HELIX_USER_AGENT: &str = "two-bot-next/1.0 (+https://two.gg)";

/// Environment variable carrying the Operator-provisioned Twitch client id.
pub const TWITCH_CLIENT_ID_ENV: &str = "TWO_TWITCH_CLIENT_ID";
/// Environment variable carrying the Operator-provisioned Twitch client secret.
pub const TWITCH_CLIENT_SECRET_ENV: &str = "TWO_TWITCH_CLIENT_SECRET";
/// Environment variable carrying the Operator-provisioned EventSub signing secret.
pub const TWITCH_WEBHOOK_SECRET_ENV: &str = "TWO_TWITCH_WEBHOOK_SECRET";

/// Helix failures. Messages name variables and statuses, never secret values.
#[derive(Debug, Error)]
pub enum HelixError {
    /// A required Operator-provisioned variable is absent.
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    /// A provisioned value is present but unusable (empty, or a signing
    /// secret outside Twitch's 10-100 ASCII-char window).
    #[error("invalid {name}: {reason}")]
    InvalidCredential {
        name: &'static str,
        reason: &'static str,
    },
    /// Pure subscription planning refused the input.
    #[error(transparent)]
    Plan(#[from] EventSubError),
    /// The token endpoint answered non-2xx.
    #[error("twitch token endpoint rejected the client credentials (status {0:?})")]
    TokenRejected(StatusCode),
    /// A Helix call answered non-2xx (after the single 401 retry, if any).
    #[error("helix {action} failed (status {status:?})")]
    HelixRejected {
        status: StatusCode,
        action: &'static str,
    },
    /// A 2xx body was not the shape the caller asked for.
    #[error("helix response was not usable: {0}")]
    BadResponse(&'static str),
    /// The subscription listing exceeded its bounded page count.
    #[error("helix subscription listing exceeded the page limit")]
    SubscriptionPageLimit,
    /// TLS, TCP or HTTP-layer failure, or a body over the byte cap.
    #[error("helix request failed: {0}")]
    Transport(String),
    /// The whole round trip exceeded [`HELIX_TIMEOUT_MS`].
    #[error("helix request exceeded the timeout")]
    Timeout,
}

/// Operator-provisioned Twitch client credentials. Both values stay inside
/// [`Secret`]: even `Debug` renders `[REDACTED]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelixCredentials {
    /// Twitch application client id (`Client-Id` header value).
    pub client_id: Secret<String>,
    /// Twitch application client secret (token endpoint only, never Helix).
    pub client_secret: Secret<String>,
}

impl HelixCredentials {
    /// Load from the environment (`TWO_TWITCH_CLIENT_ID` /
    /// `TWO_TWITCH_CLIENT_SECRET`).
    pub fn from_env() -> Result<Self, HelixError> {
        Self::from_lookup(std::env::var)
    }

    fn from_lookup(
        mut lookup: impl FnMut(&'static str) -> Result<String, std::env::VarError>,
    ) -> Result<Self, HelixError> {
        let client_id =
            lookup(TWITCH_CLIENT_ID_ENV).map_err(|_| HelixError::Missing(TWITCH_CLIENT_ID_ENV))?;
        let client_secret = lookup(TWITCH_CLIENT_SECRET_ENV)
            .map_err(|_| HelixError::Missing(TWITCH_CLIENT_SECRET_ENV))?;
        if client_id.is_empty() {
            return Err(HelixError::InvalidCredential {
                name: TWITCH_CLIENT_ID_ENV,
                reason: "value must not be empty",
            });
        }
        if client_secret.is_empty() {
            return Err(HelixError::InvalidCredential {
                name: TWITCH_CLIENT_SECRET_ENV,
                reason: "value must not be empty",
            });
        }
        Ok(Self {
            client_id: Secret::new(client_id),
            client_secret: Secret::new(client_secret),
        })
    }
}

/// Load the Operator-provisioned EventSub signing secret
/// (`TWO_TWITCH_WEBHOOK_SECRET`) for subscription creates.
pub fn webhook_secret_from_env() -> Result<WebhookSecret, HelixError> {
    webhook_secret_from_lookup(std::env::var)
}

fn webhook_secret_from_lookup(
    mut lookup: impl FnMut(&'static str) -> Result<String, std::env::VarError>,
) -> Result<WebhookSecret, HelixError> {
    let raw = lookup(TWITCH_WEBHOOK_SECRET_ENV)
        .map_err(|_| HelixError::Missing(TWITCH_WEBHOOK_SECRET_ENV))?;
    WebhookSecret::new(raw).map_err(|_| HelixError::InvalidCredential {
        name: TWITCH_WEBHOOK_SECRET_ENV,
        reason: "value must be 10-100 printable ASCII characters",
    })
}

/// A cached client-credentials app token plus its expiry instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppToken {
    /// Bearer value for the `Authorization` header. Never logged.
    pub token: Secret<String>,
    /// Unix millis when Twitch expires the token (`expires_in` from issue).
    pub expires_at_ms: i64,
}

impl AppToken {
    /// True when the token must be refreshed before the next Helix call:
    /// expired, or expiring within [`TOKEN_REFRESH_SKEW_MS`].
    #[must_use]
    pub fn needs_refresh(&self, now_ms: i64) -> bool {
        now_ms.saturating_add(TOKEN_REFRESH_SKEW_MS) >= self.expires_at_ms
    }
}

/// Parse a `POST` token response (`{"access_token","expires_in",...}`) into a
/// cached [`AppToken`]. `now_ms` is a parameter so tests pin the clock.
pub fn parse_token_response(body: &serde_json::Value, now_ms: i64) -> Result<AppToken, HelixError> {
    let token = body
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(HelixError::BadResponse(
            "token response is missing access_token",
        ))?;
    let expires_in_secs = body
        .get("expires_in")
        .and_then(serde_json::Value::as_u64)
        .filter(|secs| *secs > 0)
        .ok_or(HelixError::BadResponse(
            "token response is missing expires_in",
        ))?;
    let ttl_ms = i64::try_from(expires_in_secs.saturating_mul(1_000)).unwrap_or(i64::MAX);
    Ok(AppToken {
        token: Secret::new(token.to_owned()),
        expires_at_ms: now_ms.saturating_add(ttl_ms),
    })
}

/// `application/x-www-form-urlencoded` body for the client-credentials grant.
/// Returned as a [`Secret`] because it embeds the client secret.
#[must_use]
pub fn token_request_body(creds: &HelixCredentials) -> Secret<String> {
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "client_credentials")
        .append_pair("client_id", creds.client_id.expose())
        .append_pair("client_secret", creds.client_secret.expose())
        .finish();
    Secret::new(body)
}

/// Milliseconds since the Unix epoch (saturating; pre-epoch clocks read 0).
#[must_use]
pub fn system_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Plan the boot (or post-revocation) reconcile from all subscriptions returned
/// by [`HelixClient::list_subscriptions`]. The returned plan is deletes-first
/// via [`reconcile`].
#[must_use = "a reconcile plan does nothing until executed"]
pub fn plan_boot_actions(
    desired: &[String],
    callback: &CallbackUrl,
    existing: &[ExistingSubscription],
) -> Result<Vec<SubscriptionAction>, HelixError> {
    Ok(reconcile(desired, callback, existing)?)
}

/// One Helix/token HTTP exchange. Header values and bodies that carry secrets
/// stay inside [`Secret`], so even a `Debug` dump of the request is safe.
#[derive(Debug, Clone)]
pub struct HelixRequest {
    /// `GET`, `POST` or `DELETE`.
    pub method: Method,
    /// Fixed-host URL: [`TOKEN_URL`], the listing URL, or a URL built by
    /// [`delete_subscription_url`].
    pub url: String,
    /// `(name, value)` pairs; the value is secret-typed because
    /// `Authorization` (and `Client-Id`) must never reach a log.
    pub headers: Vec<(String, Secret<String>)>,
    /// POST body, if any.
    pub body: Option<Secret<String>>,
}

/// A buffered Helix/token HTTP answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelixResponse {
    /// Answer status.
    pub status: StatusCode,
    /// Full body, already bounded by [`HELIX_MAX_BODY_BYTES`].
    pub body: Vec<u8>,
}

/// Transport seam: production builds one hyper request per call; tests inject
/// a scripted transport. Nothing here resolves names or follows redirects —
/// both URLs are fixed Twitch constants.
pub trait HelixTransport {
    /// Issue one exchange and return the buffered answer.
    fn send(
        &self,
        request: HelixRequest,
    ) -> impl Future<Output = Result<HelixResponse, HelixError>> + Send;
}

/// Production transport: hyper 1.x over rustls with Mozilla roots, one fresh
/// client per request (no pooled connection survives the call).
#[derive(Debug, Clone, Copy, Default)]
pub struct HyperHelixTransport;

impl HyperHelixTransport {
    /// Build the hyper request. Extracted so tests prove the wire shape
    /// without standing up TLS.
    fn build_request(req: &HelixRequest) -> Result<hyper::Request<Full<Bytes>>, HelixError> {
        let mut builder = hyper::Request::builder()
            .method(req.method.clone())
            .uri(req.url.as_str())
            .header(USER_AGENT, HELIX_USER_AGENT);
        for (name, value) in &req.headers {
            builder = builder.header(name.as_str(), value.expose().as_str());
        }
        let body = req
            .body
            .as_ref()
            .map(|body| Bytes::copy_from_slice(body.expose().as_bytes()))
            .unwrap_or_default();
        builder
            .body(Full::new(body))
            .map_err(|err| HelixError::Transport(format!("helix request build failed: {err}")))
    }

    async fn send_inner(request: HelixRequest) -> Result<HelixResponse, HelixError> {
        let https = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .wrap_connector(HttpConnector::new());
        let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build(https);
        let req = Self::build_request(&request)?;
        let res = client
            .request(req)
            .await
            .map_err(|err| HelixError::Transport(format!("helix request failed: {err}")))?;
        let status = res.status();
        let mut buffered = Vec::new();
        let mut stream = BodyStream::new(res.into_body());
        use futures_util::StreamExt as _;
        while let Some(frame) = stream.next().await {
            let frame =
                frame.map_err(|err| HelixError::Transport(format!("helix body failed: {err}")))?;
            if let Ok(chunk) = frame.into_data() {
                if buffered.len().saturating_add(chunk.len()) > HELIX_MAX_BODY_BYTES {
                    return Err(HelixError::Transport(format!(
                        "helix body exceeds {HELIX_MAX_BODY_BYTES} bytes"
                    )));
                }
                buffered.extend_from_slice(&chunk);
            }
        }
        Ok(HelixResponse {
            status,
            body: buffered,
        })
    }
}

impl HelixTransport for HyperHelixTransport {
    async fn send(&self, request: HelixRequest) -> Result<HelixResponse, HelixError> {
        tokio::time::timeout(
            Duration::from_millis(HELIX_TIMEOUT_MS),
            Self::send_inner(request),
        )
        .await
        .map_err(|_| HelixError::Timeout)?
    }
}

/// What one executed subscription action did, for receiver logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// The stale/failed subscription id is gone.
    Deleted {
        /// Deleted subscription id.
        id: String,
    },
    /// The broadcaster now has a subscription on our callback.
    Created {
        /// Broadcaster the subscription was created for.
        broadcaster_user_id: String,
    },
    /// Twitch answered 409 and a follow-up listing confirmed a healthy
    /// subscription for this broadcaster on our callback.
    AlreadyExists {
        /// Broadcaster the subscription already covers.
        broadcaster_user_id: String,
    },
}

fn subscriptions_url(broadcaster_user_id: Option<&str>, cursor: Option<&str>) -> String {
    let mut query = form_urlencoded::Serializer::new(String::new());
    query.append_pair("type", "stream.online");
    if let Some(broadcaster_user_id) = broadcaster_user_id {
        query.append_pair("user_id", broadcaster_user_id);
    }
    if let Some(cursor) = cursor {
        query.append_pair("after", cursor);
    }
    format!("{HELIX_SUBSCRIPTIONS_URL}?{}", query.finish())
}

/// The Helix client: cached app token plus plan execution. `T` is the
/// transport (`HyperHelixTransport` in production, a script in tests);
/// `clock` supplies Unix millis (default [`system_millis`]) so refresh
/// decisions stay testable.
#[derive(Debug)]
pub struct HelixClient<T = HyperHelixTransport> {
    transport: T,
    creds: HelixCredentials,
    token: Option<AppToken>,
    clock: fn() -> i64,
}

impl<T: HelixTransport> HelixClient<T> {
    /// Build a client over `transport` with `creds`. No I/O happens here.
    #[must_use]
    pub fn new(transport: T, creds: HelixCredentials) -> Self {
        Self {
            transport,
            creds,
            token: None,
            clock: system_millis,
        }
    }

    /// Pin the clock (tests only): refresh decisions read this instant.
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> i64) -> Self {
        self.clock = clock;
        self
    }

    /// Return a usable app token, refreshing through the token endpoint when
    /// the cache is empty or within [`TOKEN_REFRESH_SKEW_MS`] of expiry.
    pub async fn app_token(&mut self) -> Result<Secret<String>, HelixError> {
        let now = (self.clock)();
        let fresh = self
            .token
            .as_ref()
            .is_some_and(|token| !token.needs_refresh(now));
        if !fresh {
            self.refresh_token().await?;
        }
        Ok(Secret::new(
            self.token
                .as_ref()
                .expect("refresh stores a token")
                .token
                .expose()
                .to_owned(),
        ))
    }

    /// Force a token refresh regardless of cache state (used after a 401).
    pub async fn refresh_token(&mut self) -> Result<(), HelixError> {
        let body = token_request_body(&self.creds);
        let response = self
            .transport
            .send(HelixRequest {
                method: Method::POST,
                url: TOKEN_URL.to_owned(),
                headers: vec![(
                    "content-type".to_owned(),
                    Secret::new("application/x-www-form-urlencoded".to_owned()),
                )],
                body: Some(body),
            })
            .await?;
        if !response.status.is_success() {
            return Err(HelixError::TokenRejected(response.status));
        }
        let document: serde_json::Value = serde_json::from_slice(&response.body)
            .map_err(|_| HelixError::BadResponse("token response is not JSON"))?;
        let now = (self.clock)();
        self.token = Some(parse_token_response(&document, now)?);
        Ok(())
    }

    /// Send one authed call, refreshing once and retrying on 401.
    async fn send_authed(
        &mut self,
        action: &'static str,
        mut request: HelixRequest,
    ) -> Result<HelixResponse, HelixError> {
        let token = self.app_token().await?;
        for (name, value) in [
            (
                "client-id",
                Secret::new(self.creds.client_id.expose().to_owned()),
            ),
            (
                "authorization",
                Secret::new(format!("Bearer {}", token.expose())),
            ),
        ] {
            request.headers.push((name.to_owned(), value));
        }
        let mut response = self.transport.send(request.clone()).await?;
        if response.status == StatusCode::UNAUTHORIZED {
            self.refresh_token().await?;
            let retry_token = self
                .token
                .as_ref()
                .expect("refresh stores a token")
                .token
                .expose()
                .to_owned();
            let mut retry = request;
            retry
                .headers
                .retain(|(name, _)| name != "client-id" && name != "authorization");
            retry.headers.push((
                "client-id".to_owned(),
                Secret::new(self.creds.client_id.expose().to_owned()),
            ));
            retry.headers.push((
                "authorization".to_owned(),
                Secret::new(format!("Bearer {retry_token}")),
            ));
            response = self.transport.send(retry).await?;
        }
        if !response.status.is_success() {
            return Err(HelixError::HelixRejected {
                status: response.status,
                action,
            });
        }
        Ok(response)
    }

    /// List every `stream.online` subscription page, parsed to v1 webhooks via
    /// [`parse_subscriptions`].
    pub async fn list_subscriptions(&mut self) -> Result<Vec<ExistingSubscription>, HelixError> {
        self.list_subscriptions_with_user_id(None).await
    }

    async fn list_subscriptions_for_broadcaster(
        &mut self,
        broadcaster_user_id: &str,
    ) -> Result<Vec<ExistingSubscription>, HelixError> {
        self.list_subscriptions_with_user_id(Some(broadcaster_user_id))
            .await
    }

    async fn list_subscriptions_with_user_id(
        &mut self,
        broadcaster_user_id: Option<&str>,
    ) -> Result<Vec<ExistingSubscription>, HelixError> {
        let mut subscriptions = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();

        for page in 0..HELIX_MAX_SUBSCRIPTION_PAGES {
            let response = self
                .send_authed(
                    "list subscriptions",
                    HelixRequest {
                        method: Method::GET,
                        url: subscriptions_url(broadcaster_user_id, cursor.as_deref()),
                        headers: vec![(
                            "accept".to_owned(),
                            Secret::new("application/json".to_owned()),
                        )],
                        body: None,
                    },
                )
                .await?;
            let document: serde_json::Value = serde_json::from_slice(&response.body)
                .map_err(|_| HelixError::BadResponse("subscription listing is not JSON"))?;
            subscriptions.extend(parse_subscriptions(&document));

            let next_cursor = match document.get("pagination") {
                Some(pagination) => match pagination.get("cursor") {
                    Some(serde_json::Value::String(value)) if !value.is_empty() => {
                        Some(value.as_str())
                    }
                    Some(serde_json::Value::Null) | None => None,
                    Some(_) => {
                        return Err(HelixError::BadResponse(
                            "subscription pagination cursor is not usable",
                        ));
                    }
                },
                None => None,
            };
            let Some(next_cursor) = next_cursor else {
                return Ok(subscriptions);
            };
            if !seen_cursors.insert(next_cursor.to_owned()) {
                return Err(HelixError::BadResponse(
                    "subscription pagination cursor repeated",
                ));
            }
            if page + 1 == HELIX_MAX_SUBSCRIPTION_PAGES {
                return Err(HelixError::SubscriptionPageLimit);
            }
            cursor = Some(next_cursor.to_owned());
        }

        Err(HelixError::SubscriptionPageLimit)
    }

    /// Execute a [`reconcile`] plan in order (deletes first). A create that
    /// answers 409 reports [`ActionOutcome::AlreadyExists`] only after a
    /// follow-up listing confirms a healthy subscription on our callback.
    pub async fn execute_actions(
        &mut self,
        actions: &[SubscriptionAction],
        callback: &CallbackUrl,
        secret: &WebhookSecret,
    ) -> Result<Vec<ActionOutcome>, HelixError> {
        let mut outcomes = Vec::with_capacity(actions.len());
        for action in actions {
            match action {
                SubscriptionAction::Delete { id } => {
                    let response = self
                        .send_authed(
                            "delete subscription",
                            HelixRequest {
                                method: Method::DELETE,
                                url: delete_subscription_url(id)?,
                                headers: Vec::new(),
                                body: None,
                            },
                        )
                        .await;
                    match response {
                        Ok(_) => outcomes.push(ActionOutcome::Deleted { id: id.clone() }),
                        Err(HelixError::HelixRejected { status, .. })
                            if status == StatusCode::NOT_FOUND =>
                        {
                            // Already gone: the reconciled end state holds.
                            outcomes.push(ActionOutcome::Deleted { id: id.clone() })
                        }
                        Err(err) => return Err(err),
                    }
                }
                SubscriptionAction::Create {
                    broadcaster_user_id,
                } => {
                    let body = create_subscription_body(broadcaster_user_id, callback, secret)?;
                    let request = HelixRequest {
                        method: Method::POST,
                        url: HELIX_SUBSCRIPTIONS_URL.to_owned(),
                        headers: vec![(
                            "content-type".to_owned(),
                            Secret::new("application/json".to_owned()),
                        )],
                        body: Some(body),
                    };
                    let response = self.send_authed("create subscription", request).await;
                    match response {
                        Ok(_) => outcomes.push(ActionOutcome::Created {
                            broadcaster_user_id: broadcaster_user_id.clone(),
                        }),
                        Err(HelixError::HelixRejected { status, .. })
                            if status == StatusCode::CONFLICT =>
                        {
                            let existing = self
                                .list_subscriptions_for_broadcaster(broadcaster_user_id)
                                .await?;
                            if existing.iter().any(|subscription| {
                                subscription.broadcaster_user_id.as_str()
                                    == broadcaster_user_id.as_str()
                                    && subscription.callback == callback.as_str()
                                    && subscription.healthy()
                            }) {
                                outcomes.push(ActionOutcome::AlreadyExists {
                                    broadcaster_user_id: broadcaster_user_id.clone(),
                                });
                            } else {
                                return Err(HelixError::HelixRejected {
                                    status: StatusCode::CONFLICT,
                                    action: "create subscription (conflict not verified)",
                                });
                            }
                        }
                        Err(err) => return Err(err),
                    }
                }
            }
        }
        Ok(outcomes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    fn vars<'a>(
        pairs: &'a [(&'a str, &'a str)],
    ) -> impl FnMut(&'static str) -> Result<String, std::env::VarError> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
                .ok_or(std::env::VarError::NotPresent)
        }
    }

    fn fixture_creds() -> HelixCredentials {
        HelixCredentials::from_lookup(vars(&[
            (TWITCH_CLIENT_ID_ENV, "fixture-client-id"),
            (TWITCH_CLIENT_SECRET_ENV, "fixture-client-secret"),
        ]))
        .expect("fixture credentials must parse")
    }

    fn callback() -> CallbackUrl {
        CallbackUrl::new("https://hooks.example.com/twitch").expect("fixture callback must parse")
    }

    fn webhook_secret() -> WebhookSecret {
        WebhookSecret::new("fixture-signing-secret-01".to_owned())
            .expect("fixture secret must parse")
    }

    #[test]
    fn credentials_require_both_env_vars() {
        let err = HelixCredentials::from_lookup(vars(&[])).expect_err("missing id must fail");
        assert!(matches!(err, HelixError::Missing(TWITCH_CLIENT_ID_ENV)));
        let err = HelixCredentials::from_lookup(vars(&[(TWITCH_CLIENT_ID_ENV, "id")]))
            .expect_err("missing secret must fail");
        assert!(matches!(err, HelixError::Missing(TWITCH_CLIENT_SECRET_ENV)));
        for (id, secret) in [("", "secret"), ("id", "")] {
            let err = HelixCredentials::from_lookup(vars(&[
                (TWITCH_CLIENT_ID_ENV, id),
                (TWITCH_CLIENT_SECRET_ENV, secret),
            ]))
            .expect_err("empty credential must fail");
            assert!(matches!(err, HelixError::InvalidCredential { .. }));
        }
    }

    #[test]
    fn debug_never_renders_credential_values() {
        let creds = fixture_creds();
        for output in [format!("{creds:?}"), format!("{creds:#?}")] {
            for secret in ["fixture-client-id", "fixture-client-secret"] {
                assert!(!output.contains(secret), "leaked into {output:?}");
            }
            assert!(output.contains("[REDACTED]"));
        }
    }

    #[test]
    fn webhook_secret_loader_validates() {
        let err = webhook_secret_from_lookup(vars(&[])).expect_err("missing secret must fail");
        assert!(matches!(
            err,
            HelixError::Missing(TWITCH_WEBHOOK_SECRET_ENV)
        ));
        let err = webhook_secret_from_lookup(vars(&[(TWITCH_WEBHOOK_SECRET_ENV, "short")]))
            .expect_err("short secret must fail");
        assert!(matches!(err, HelixError::InvalidCredential { .. }));
        let secret = webhook_secret_from_lookup(vars(&[(TWITCH_WEBHOOK_SECRET_ENV, "0123456789")]))
            .expect("10-char secret must parse");
        assert_eq!(
            format!("{secret:?}"),
            format!("{:?}", WebhookSecret::new("0123456789".to_owned()).unwrap())
        );
    }

    #[test]
    fn token_response_parses_expiry() {
        let body = serde_json::json!({
            "access_token": "fixture-token",
            "expires_in": 3600,
            "token_type": "bearer",
        });
        let token = parse_token_response(&body, 1_000).expect("valid token must parse");
        assert_eq!(token.expires_at_ms, 1_000 + 3_600_000);
        assert!(!token.needs_refresh(1_000));
        assert!(!token.needs_refresh(1_000 + 3_600_000 - TOKEN_REFRESH_SKEW_MS - 1));
        assert!(token.needs_refresh(1_000 + 3_600_000 - TOKEN_REFRESH_SKEW_MS));
        assert!(token.needs_refresh(1_000 + 3_600_000));
        assert_eq!(format!("{:?}", token.token), "[REDACTED]");
    }

    #[test]
    fn token_response_rejects_shapes_without_bearer_material() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({"access_token": "", "expires_in": 3600}),
            serde_json::json!({"access_token": "t"}),
            serde_json::json!({"access_token": "t", "expires_in": 0}),
        ] {
            assert!(
                parse_token_response(&body, 0).is_err(),
                "must reject {body}"
            );
        }
    }

    #[test]
    fn token_request_body_is_urlencoded_and_secret_typed() {
        let body = token_request_body(&fixture_creds());
        assert_eq!(
            body.expose(),
            "grant_type=client_credentials&client_id=fixture-client-id&client_secret=fixture-client-secret"
        );
        assert_eq!(format!("{body:?}"), "[REDACTED]");
    }

    #[test]
    fn boot_plan_uses_subscriptions_accumulated_from_all_pages() {
        let pages = [
            serde_json::json!({
                "data": [{
                    "id": "stale-id",
                    "status": "enabled",
                    "type": "stream.online",
                    "version": "1",
                    "condition": {"broadcaster_user_id": "111"},
                    "transport": {"method": "webhook", "callback": "https://hooks.example.com/twitch"},
                }]
            }),
            serde_json::json!({
                "data": [
                    {
                        "id": "second-stale-id",
                        "status": "enabled",
                        "type": "stream.online",
                        "version": "1",
                        "condition": {"broadcaster_user_id": "333"},
                        "transport": {"method": "webhook", "callback": "https://hooks.example.com/twitch"},
                    },
                    {
                        "id": "other-callback-id",
                        "status": "enabled",
                        "type": "stream.online",
                        "version": "1",
                        "condition": {"broadcaster_user_id": "222"},
                        "transport": {"method": "webhook", "callback": "https://other.example.com/hook"},
                    },
                ]
            }),
        ];
        let existing: Vec<_> = pages.iter().flat_map(parse_subscriptions).collect();
        let plan = plan_boot_actions(&["222".to_owned()], &callback(), &existing)
            .expect("plan must build from accumulated pages");
        // Both stale subscriptions are on our callback and must be deleted.
        // The foreign subscription is not ours, so create the requested one.
        assert_eq!(
            plan,
            vec![
                SubscriptionAction::Delete {
                    id: "second-stale-id".to_owned()
                },
                SubscriptionAction::Delete {
                    id: "stale-id".to_owned()
                },
                SubscriptionAction::Create {
                    broadcaster_user_id: "222".to_owned()
                },
            ]
        );
    }

    /// Scripted transport: canned answers in order, records requests.
    struct ScriptTransport {
        answers: Mutex<VecDeque<Result<HelixResponse, HelixError>>>,
        seen: Mutex<Vec<HelixRequest>>,
    }

    impl ScriptTransport {
        fn new(answers: Vec<Result<HelixResponse, HelixError>>) -> Self {
            Self {
                answers: Mutex::new(answers.into()),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn token_answer(token: &str, expires_in: u64) -> Result<HelixResponse, HelixError> {
            Ok(HelixResponse {
                status: StatusCode::OK,
                body: serde_json::json!({
                    "access_token": token,
                    "expires_in": expires_in,
                })
                .to_string()
                .into_bytes(),
            })
        }

        fn seen_urls(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|req| req.url.clone())
                .collect()
        }

        fn auth_headers(&self, index: usize) -> Vec<(String, String)> {
            self.seen.lock().unwrap()[index]
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.expose().clone()))
                .collect()
        }
    }

    impl HelixTransport for ScriptTransport {
        async fn send(&self, request: HelixRequest) -> Result<HelixResponse, HelixError> {
            self.seen.lock().unwrap().push(request);
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("script must answer every call")
        }
    }

    fn ok_empty() -> Result<HelixResponse, HelixError> {
        Ok(HelixResponse {
            status: StatusCode::NO_CONTENT,
            body: Vec::new(),
        })
    }

    #[test]
    fn request_debug_redacts_header_values_and_body() {
        let req = HelixRequest {
            method: Method::POST,
            url: TOKEN_URL.to_owned(),
            headers: vec![(
                "authorization".to_owned(),
                Secret::new("Bearer hunter2".to_owned()),
            )],
            body: Some(Secret::new("grant_type=client_credentials".to_owned())),
        };
        let rendered = format!("{req:?}");
        assert!(!rendered.contains("hunter2"), "leaked into {rendered:?}");
        assert!(rendered.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn token_caches_until_skew_then_refreshes() {
        let transport = ScriptTransport::new(vec![
            ScriptTransport::token_answer("first", 3_600),
            ScriptTransport::token_answer("second", 3_600),
        ]);
        let mut client = HelixClient::new(transport, fixture_creds()).with_clock(|| 0);
        let first = client.app_token().await.expect("first issue must work");
        assert_eq!(first.expose(), "first");
        // Still fresh: no second token call.
        let again = client.app_token().await.expect("cache must hold");
        assert_eq!(again.expose(), "first");
        // Past the skew: refreshes.
        let mut client = client.with_clock(|| 3_600_000 - TOKEN_REFRESH_SKEW_MS);
        let second = client.app_token().await.expect("refresh must work");
        assert_eq!(second.expose(), "second");
    }

    #[tokio::test]
    async fn listing_sends_bearer_and_client_id() {
        let transport = ScriptTransport::new(vec![
            ScriptTransport::token_answer("listed-token", 3_600),
            Ok(HelixResponse {
                status: StatusCode::OK,
                body: serde_json::json!({"data": [], "pagination": {}})
                    .to_string()
                    .into_bytes(),
            }),
        ]);
        let mut client = HelixClient::new(transport, fixture_creds()).with_clock(|| 0);
        let subs = client
            .list_subscriptions()
            .await
            .expect("listing must work");
        assert!(subs.is_empty());
        let transport = &client.transport;
        assert_eq!(transport.seen_urls().len(), 2);
        assert_eq!(
            transport.seen_urls()[1],
            format!("{HELIX_SUBSCRIPTIONS_URL}?type=stream.online")
        );
        let headers = transport.auth_headers(1);
        assert!(headers.contains(&("authorization".to_owned(), "Bearer listed-token".to_owned())));
        assert!(headers.contains(&("client-id".to_owned(), "fixture-client-id".to_owned())));
    }

    #[tokio::test]
    async fn listing_follows_cursor_across_pages() {
        let subscription = |id: &str, broadcaster: &str| {
            serde_json::json!({
                "id": id,
                "status": "enabled",
                "type": "stream.online",
                "version": "1",
                "condition": {"broadcaster_user_id": broadcaster},
                "transport": {
                    "method": "webhook",
                    "callback": "https://hooks.example.com/twitch"
                }
            })
        };
        let transport = ScriptTransport::new(vec![
            ScriptTransport::token_answer("listed-token", 3_600),
            Ok(HelixResponse {
                status: StatusCode::OK,
                body: serde_json::json!({
                    "data": [subscription("first", "111")],
                    "pagination": {"cursor": "cursor+/="}
                })
                .to_string()
                .into_bytes(),
            }),
            Ok(HelixResponse {
                status: StatusCode::OK,
                body: serde_json::json!({
                    "data": [subscription("second", "222")],
                    "pagination": {}
                })
                .to_string()
                .into_bytes(),
            }),
        ]);
        let mut client = HelixClient::new(transport, fixture_creds()).with_clock(|| 0);
        let subscriptions = client
            .list_subscriptions()
            .await
            .expect("all pages must be listed");
        assert_eq!(
            subscriptions
                .iter()
                .map(|sub| sub.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(
            client.transport.seen_urls(),
            [
                TOKEN_URL.to_owned(),
                format!("{HELIX_SUBSCRIPTIONS_URL}?type=stream.online"),
                format!("{HELIX_SUBSCRIPTIONS_URL}?type=stream.online&after=cursor%2B%2F%3D"),
            ]
        );
    }

    #[tokio::test]
    async fn listing_fails_closed_when_page_limit_is_exceeded() {
        let mut answers = vec![ScriptTransport::token_answer("listed-token", 3_600)];
        for page in 0..HELIX_MAX_SUBSCRIPTION_PAGES {
            answers.push(Ok(HelixResponse {
                status: StatusCode::OK,
                body: serde_json::json!({
                    "data": [],
                    "pagination": {"cursor": format!("cursor-{page}")}
                })
                .to_string()
                .into_bytes(),
            }));
        }
        let transport = ScriptTransport::new(answers);
        let mut client = HelixClient::new(transport, fixture_creds()).with_clock(|| 0);
        assert!(matches!(
            client.list_subscriptions().await,
            Err(HelixError::SubscriptionPageLimit)
        ));
        assert_eq!(
            client.transport.seen_urls().len(),
            HELIX_MAX_SUBSCRIPTION_PAGES + 1
        );
    }

    #[tokio::test]
    async fn unauthorized_refreshes_once_and_retries() {
        let transport = ScriptTransport::new(vec![
            ScriptTransport::token_answer("stale", 3_600),
            Ok(HelixResponse {
                status: StatusCode::UNAUTHORIZED,
                body: Vec::new(),
            }),
            ScriptTransport::token_answer("fresh", 3_600),
            Ok(HelixResponse {
                status: StatusCode::OK,
                body: serde_json::json!({"data": []}).to_string().into_bytes(),
            }),
        ]);
        let mut client = HelixClient::new(transport, fixture_creds()).with_clock(|| 0);
        client
            .list_subscriptions()
            .await
            .expect("retry must succeed");
        let transport = &client.transport;
        assert_eq!(transport.seen_urls().len(), 4);
        let headers = transport.auth_headers(3);
        assert!(headers.contains(&("authorization".to_owned(), "Bearer fresh".to_owned())));
    }

    #[tokio::test]
    async fn execute_runs_deletes_before_creates_and_verifies_409() {
        let existing = serde_json::json!({
            "data": [{
                "id": "existing-id",
                "status": "enabled",
                "type": "stream.online",
                "version": "1",
                "condition": {"broadcaster_user_id": "222"},
                "transport": {
                    "method": "webhook",
                    "callback": "https://hooks.example.com/twitch"
                }
            }],
            "pagination": {}
        });
        let transport = ScriptTransport::new(vec![
            ScriptTransport::token_answer("exec-token", 3_600),
            ok_empty(), // DELETE stale-id -> 204
            Ok(HelixResponse {
                status: StatusCode::CONFLICT,
                body: Vec::new(),
            }), // POST create -> 409
            Ok(HelixResponse {
                status: StatusCode::OK,
                body: existing.to_string().into_bytes(),
            }), // GET broadcaster subscriptions -> confirms healthy callback
        ]);
        let mut client = HelixClient::new(transport, fixture_creds()).with_clock(|| 0);
        let outcomes = client
            .execute_actions(
                &[
                    SubscriptionAction::Delete {
                        id: "stale-id".to_owned(),
                    },
                    SubscriptionAction::Create {
                        broadcaster_user_id: "222".to_owned(),
                    },
                ],
                &callback(),
                &webhook_secret(),
            )
            .await
            .expect("verified 409 must be treated as already existing");
        assert_eq!(
            outcomes,
            vec![
                ActionOutcome::Deleted {
                    id: "stale-id".to_owned()
                },
                ActionOutcome::AlreadyExists {
                    broadcaster_user_id: "222".to_owned()
                },
            ]
        );
        let transport = &client.transport;
        let urls = transport.seen_urls();
        assert_eq!(urls.len(), 4);
        assert_eq!(urls[1], format!("{HELIX_SUBSCRIPTIONS_URL}?id=stale-id"));
        assert_eq!(urls[2], HELIX_SUBSCRIPTIONS_URL);
        assert_eq!(
            urls[3],
            format!("{HELIX_SUBSCRIPTIONS_URL}?type=stream.online&user_id=222")
        );
        // The create body carries our callback and condition, never logged here.
        let created = &transport.seen.lock().unwrap()[2];
        let document: serde_json::Value =
            serde_json::from_str(created.body.as_ref().expect("create has a body").expose())
                .expect("create body must be JSON");
        assert_eq!(document["condition"]["broadcaster_user_id"], "222");
        assert_eq!(
            document["transport"]["callback"],
            "https://hooks.example.com/twitch"
        );
    }

    #[tokio::test]
    async fn create_conflict_is_not_success_for_foreign_or_failed_subscription() {
        for (callback_url, status) in [
            ("https://other.example.com/hook", "enabled"),
            (
                "https://hooks.example.com/twitch",
                "notification_failures_exceeded",
            ),
        ] {
            let existing = serde_json::json!({
                "data": [{
                    "id": "existing-id",
                    "status": status,
                    "type": "stream.online",
                    "version": "1",
                    "condition": {"broadcaster_user_id": "222"},
                    "transport": {"method": "webhook", "callback": callback_url}
                }],
                "pagination": {}
            });
            let transport = ScriptTransport::new(vec![
                ScriptTransport::token_answer("exec-token", 3_600),
                Ok(HelixResponse {
                    status: StatusCode::CONFLICT,
                    body: Vec::new(),
                }),
                Ok(HelixResponse {
                    status: StatusCode::OK,
                    body: existing.to_string().into_bytes(),
                }),
            ]);
            let mut client = HelixClient::new(transport, fixture_creds()).with_clock(|| 0);
            let error = client
                .execute_actions(
                    &[SubscriptionAction::Create {
                        broadcaster_user_id: "222".to_owned(),
                    }],
                    &callback(),
                    &webhook_secret(),
                )
                .await
                .expect_err("unverified create conflict must fail");
            assert!(matches!(
                error,
                HelixError::HelixRejected {
                    status: StatusCode::CONFLICT,
                    ..
                }
            ));
        }
    }

    #[tokio::test]
    async fn token_rejection_surfaces_without_secret() {
        let transport = ScriptTransport::new(vec![Ok(HelixResponse {
            status: StatusCode::FORBIDDEN,
            body: b"wrong client".to_vec(),
        })]);
        let mut client = HelixClient::new(transport, fixture_creds()).with_clock(|| 0);
        let err = client.app_token().await.expect_err("bad creds must fail");
        let rendered = format!("{err}");
        assert!(
            !rendered.contains("fixture-client-secret"),
            "leaked into {rendered:?}"
        );
        assert!(matches!(err, HelixError::TokenRejected(_)));
    }

    #[test]
    fn production_request_builder_sets_headers_without_tls() {
        let req = HelixRequest {
            method: Method::GET,
            url: subscriptions_url(None, None),
            headers: vec![(
                "authorization".to_owned(),
                Secret::new("Bearer prove-no-tls".to_owned()),
            )],
            body: None,
        };
        let built = HyperHelixTransport::build_request(&req).expect("build must work");
        assert_eq!(built.method(), Method::GET);
        assert_eq!(built.headers()["authorization"], "Bearer prove-no-tls");
        assert_eq!(built.headers()[USER_AGENT], HELIX_USER_AGENT);
    }
}
