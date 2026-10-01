//! Callable internal-action effects, not an authenticated HTTP receiver.
//!
//! The caller must authorize and commit its durable execution claim first.
//! Only `announcement.post` is implemented here; core feature flags are not
//! executor capabilities. No runtime flags, stores or listeners are installed.

use bytes::Bytes;
use http::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER, USER_AGENT};
use http_body_util::{BodyExt, Full, Limited};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use twilight_http::{request::TryIntoRequest, Client as TwilightClient};
use twilight_model::{
    channel::message::AllowedMentions,
    id::{marker::ChannelMarker, marker::MessageMarker, Id},
};
use two_bot_core::internal_actions::{is_snowflake, validate_announcement, ErrorCode};
use two_bot_core::send_admission::{
    is_loopback_http, AdmissionError, SendAdmission, SendCooldown, TokenKey,
};

pub const SUPPORTED_ACTIONS: &[&str] = &["announcement.post"];
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const BOT_USER_AGENT: &str = concat!(
    "DiscordBot (https://github.com/TogetherWeOwn/two-bot-next, ",
    env!("CARGO_PKG_VERSION"),
    ")"
);

type HttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// No free-text provider errors or request values may enter this contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    Malformed,
    ActionNotAllowed,
    InvalidChannelConfiguration,
    LocalConfiguration,
    SendAdmissionBlocked,
    DiscordRejected,
}

/// Apply before any new intent in the caller's shared token governor. Channel
/// scope deliberately covers all buckets on this major resource; bucket strings
/// and provider text never escape. Missing/ambiguous scope is token-wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CooldownScope {
    Global,
    Channel(Id<ChannelMarker>),
}

/// A 429 proves no effect, but also constrains later, independent intents.
/// `None` means timing was unavailable: pause this scope pending reconciliation,
/// not a guessed short delay. Never retry the original POST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RateLimitCooldown {
    pub scope: CooldownScope,
    pub retry_after_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownReason {
    Timeout,
    Transport,
    Upstream,
    InvalidResponse,
}

/// Cache-safe receipt. Never return Discord's message model (it includes content).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AnnouncementReceipt {
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
}

impl AnnouncementReceipt {
    #[must_use]
    pub const fn channel_id(self) -> Id<ChannelMarker> {
        self.channel_id
    }

    #[must_use]
    pub const fn message_id(self) -> Id<MessageMarker> {
        self.message_id
    }
}

/// `NoEffect` proves local refusal or a definitive Discord rejection. `Unknown`
/// must retain the claim for reconciliation, never release it or retry creation.
/// The caller persists a terminal receipt/refusal before replying to its client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Posted(AnnouncementReceipt),
    NoEffect(Refusal),
    RateLimited(RateLimitCooldown),
    Unknown(UnknownReason),
}

/// Uses Twilight's validated request builder, but deliberately does NOT await
/// its ResponseFuture: Twilight retries 429/5xx internally. This transport has
/// no status retries, redirects, or cancelled pooled-connection retries.
/// Construction assumes the application's rustls provider is already installed.
pub struct AnnouncementExecutor {
    twilight: Arc<TwilightClient>,
    http: HttpClient,
    channel_keys: HashMap<String, String>,
    timeout: Duration,
    api_origin: String,
    admission: Option<Arc<dyn SendAdmission>>,
}

impl AnnouncementExecutor {
    #[must_use]
    pub fn new(twilight: Arc<TwilightClient>, channel_keys: HashMap<String, String>) -> Self {
        let https = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        let http = Client::builder(TokioExecutor::new())
            .retry_canceled_requests(false)
            .build(https);
        Self {
            twilight,
            http,
            channel_keys,
            timeout: Duration::from_secs(10),
            api_origin: "https://discord.com".to_owned(),
            admission: None,
        }
    }

    /// Production bootstrap: bind this executor to the credential's shared
    /// database lane. `new` without admission refuses all non-loopback sends.
    pub fn with_admission(
        twilight: Arc<TwilightClient>,
        channel_keys: HashMap<String, String>,
        admission: Arc<dyn SendAdmission>,
    ) -> Result<Self, AdmissionError> {
        let token = twilight.token().ok_or(AdmissionError::Configuration)?;
        if admission.token_key() != &TokenKey::for_bot_token(token)? {
            return Err(AdmissionError::Configuration);
        }
        let mut executor = Self::new(twilight, channel_keys);
        executor.admission = Some(admission);
        Ok(executor)
    }

    #[must_use]
    pub fn supports(action: &str) -> bool {
        SUPPORTED_ACTIONS.contains(&action)
    }

    /// Authorize first; this method intentionally owns neither the store claim
    /// nor its audit/finalization. Caller cancellation after invocation is also
    /// an unknown outcome, even if this method never returns.
    pub async fn execute(&self, action: &str, body: &Map<String, Value>) -> ExecutionOutcome {
        if !Self::supports(action) {
            return ExecutionOutcome::NoEffect(Refusal::ActionNotAllowed);
        }
        let outcome = self.post(body).await;
        // Only closed enums: no channel key, input, request, token or error source.
        tracing::info!(action = "announcement.post", outcome = ?outcome, "internal action completed");
        outcome
    }

    async fn post(&self, body: &Map<String, Value>) -> ExecutionOutcome {
        let channel = match validate_announcement(body, &self.channel_keys) {
            Ok(id) => id,
            Err(error) => {
                // ActionError's text may echo an untrusted channel key. Drop it.
                let refusal = if error.code == ErrorCode::ActionNotAllowed {
                    Refusal::ActionNotAllowed
                } else {
                    Refusal::Malformed
                };
                return ExecutionOutcome::NoEffect(refusal);
            }
        };
        // Maps may be supplied directly, not only via build_channel_keys. Domain
        // snowflake shape alone also admits values above u64::MAX; fail closed.
        let channel_id = match parse_id::<ChannelMarker>(channel) {
            Some(id) => id,
            None => return ExecutionOutcome::NoEffect(Refusal::InvalidChannelConfiguration),
        };
        let content = body["body"].as_str().expect("core validated body");
        let mentions = AllowedMentions::default();
        let request = match self
            .twilight
            .create_message(channel_id)
            .content(content)
            .allowed_mentions(Some(&mentions))
            .try_into_request()
        {
            Ok(request) => request,
            Err(_) => return ExecutionOutcome::NoEffect(Refusal::Malformed),
        };
        let Some(token) = self.twilight.token() else {
            return ExecutionOutcome::NoEffect(Refusal::LocalConfiguration);
        };
        let mut authorization = match HeaderValue::from_str(token) {
            Ok(header) => header,
            Err(_) => return ExecutionOutcome::NoEffect(Refusal::LocalConfiguration),
        };
        authorization.set_sensitive(true);
        let outbound = match http::Request::builder()
            .method(request.method().name())
            .uri(format!("{}/api/v10/{}", self.api_origin, request.path()))
            .header(AUTHORIZATION, authorization)
            .header(CONTENT_TYPE, "application/json")
            .header(USER_AGENT, BOT_USER_AGENT)
            .body(Full::new(Bytes::copy_from_slice(
                request.body().unwrap_or_default(),
            ))) {
            Ok(request) => request,
            Err(_) => return ExecutionOutcome::NoEffect(Refusal::LocalConfiguration),
        };

        let deadline = tokio::time::Instant::now() + self.timeout;
        let permit = match &self.admission {
            Some(admission) => match tokio::time::timeout_at(deadline, admission.admit()).await {
                Ok(Ok(permit)) => Some(permit),
                _ => return ExecutionOutcome::NoEffect(Refusal::SendAdmissionBlocked),
            },
            None if is_loopback_http(&self.api_origin) => None,
            None => return ExecutionOutcome::NoEffect(Refusal::LocalConfiguration),
        };
        let outcome = self.post_admitted(outbound, channel_id, deadline).await;
        if let Some(permit) = permit {
            let cooldown = match outcome {
                ExecutionOutcome::RateLimited(cooldown) => Some(match cooldown.retry_after_ms {
                    Some(ms) => SendCooldown::FiniteMs(ms),
                    None => SendCooldown::Indefinite,
                }),
                _ => None,
            };
            // Unknown/cancelled effects retain the admission row, not just the
            // receiver claim. A definitive 429 stays no-effect even if storage
            // fails: the already-committed occupied row blocks other sends.
            if !matches!(outcome, ExecutionOutcome::Unknown(_))
                && !matches!(
                    tokio::time::timeout_at(deadline, permit.complete(cooldown)).await,
                    Ok(Ok(()))
                )
            {
                tracing::warn!("internal-action admission completion unavailable; lane held");
            }
        }
        outcome
    }

    async fn post_admitted(
        &self,
        outbound: http::Request<Full<Bytes>>,
        channel_id: Id<ChannelMarker>,
        deadline: tokio::time::Instant,
    ) -> ExecutionOutcome {
        let response = match tokio::time::timeout_at(deadline, self.http.request(outbound)).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => return ExecutionOutcome::Unknown(UnknownReason::Transport),
            Err(_) => return ExecutionOutcome::Unknown(UnknownReason::Timeout),
        };
        match response.status().as_u16() {
            // Narrow confirmed-rejection allowlist. 408, other statuses,
            // redirects and 5xx are NOT proof of no message being created.
            400 | 401 | 403 | 404 | 405 | 413 | 415 | 422 => {
                return ExecutionOutcome::NoEffect(Refusal::DiscordRejected);
            }
            429 => {
                return ExecutionOutcome::RateLimited(
                    rate_limit_cooldown(response, channel_id, deadline).await,
                );
            }
            200 | 201 => {}
            _ => return ExecutionOutcome::Unknown(UnknownReason::Upstream),
        }
        let bytes = match tokio::time::timeout_at(
            deadline,
            Limited::new(response.into_body(), MAX_RESPONSE_BYTES).collect(),
        )
        .await
        {
            Ok(Ok(body)) => body.to_bytes(),
            Ok(Err(_)) => return ExecutionOutcome::Unknown(UnknownReason::InvalidResponse),
            Err(_) => return ExecutionOutcome::Unknown(UnknownReason::Timeout),
        };
        // Deserialize only scalar IDs; do not retain content or OAuth fields.
        let wire: MessageIds = match serde_json::from_slice(&bytes) {
            Ok(ids) => ids,
            Err(_) => return ExecutionOutcome::Unknown(UnknownReason::InvalidResponse),
        };
        let Some(message_id) = parse_id::<MessageMarker>(&wire.id) else {
            return ExecutionOutcome::Unknown(UnknownReason::InvalidResponse);
        };
        if parse_id::<ChannelMarker>(&wire.channel_id) != Some(channel_id) {
            return ExecutionOutcome::Unknown(UnknownReason::InvalidResponse);
        }
        ExecutionOutcome::Posted(AnnouncementReceipt {
            channel_id,
            message_id,
        })
    }
}

async fn rate_limit_cooldown(
    response: http::Response<hyper::body::Incoming>,
    channel_id: Id<ChannelMarker>,
    deadline: tokio::time::Instant,
) -> RateLimitCooldown {
    let headers = response.headers();
    let header_delay = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .and_then(delay_ms);
    let header_global = headers.contains_key("x-ratelimit-global")
        || headers
            .get("x-ratelimit-scope")
            .is_some_and(|value| value == "global");
    let header_channel = headers
        .get("x-ratelimit-scope")
        .is_some_and(|value| value == "user" || value == "shared");
    // The status already proved no effect. Slow, broken or oversized error
    // bodies must not discard known headers or turn this into an unknown effect.
    let body = tokio::time::timeout_at(
        deadline,
        Limited::new(response.into_body(), MAX_RESPONSE_BYTES).collect(),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .and_then(|body| serde_json::from_slice::<RateLimitWire>(&body.to_bytes()).ok());
    let scope = if header_global || body.as_ref().is_some_and(|body| body.global == Some(true)) {
        CooldownScope::Global
    } else if header_channel || body.as_ref().is_some_and(|body| body.global == Some(false)) {
        CooldownScope::Channel(channel_id)
    } else {
        CooldownScope::Global
    };
    let body_delay = body.and_then(|body| body.retry_after).and_then(delay_ms);
    RateLimitCooldown {
        scope,
        // Never shorten a valid cooldown if header and body disagree.
        retry_after_ms: header_delay.max(body_delay),
    }
}

fn delay_ms(seconds: f64) -> Option<u64> {
    let milliseconds = (seconds * 1000.0).ceil();
    // Do not cap long waits downward or interpret invalid timing as zero.
    (seconds.is_finite() && seconds >= 0.0 && milliseconds < u64::MAX as f64)
        .then_some(milliseconds as u64)
}

#[derive(Deserialize)]
struct RateLimitWire {
    retry_after: Option<f64>,
    global: Option<bool>,
}

#[derive(Deserialize)]
struct MessageIds {
    id: String,
    channel_id: String,
}

fn parse_id<T>(value: &str) -> Option<Id<T>> {
    if !is_snowflake(value) {
        return None;
    }
    let number = value.parse::<u64>().ok()?;
    if number.to_string() != value {
        return None;
    }
    Id::new_checked(number)
}

#[cfg(test)]
mod tests;
