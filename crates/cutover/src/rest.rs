//! Discord REST seam for the cutover tools.
//!
//! Ports `src/discord/rest.ts` onto twilight-http. Semantics preserved: a
//! 110ms pacing floor between requests (Discord allows 50/s globally; the
//! tools use far less), 403/404 → `None` (an unreadable channel is a normal
//! condition, not an abort), 429 backs off, 5xx retries with exponential
//! backoff (≤4 attempts). `requests` counts calls so each run reports its
//! own Discord cost.
//!
//! `proxy_url` overrides the API host (twilight's `ClientBuilder::proxy`,
//! the analogue of legacy `DISCORD_API_BASE`): verification runs point the
//! tools at a local mock instead of Discord.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use twilight_http::error::{Error as TwilightError, ErrorType};
use twilight_http::Client;
use twilight_model::channel::message::Message;
use twilight_model::channel::Channel;
use twilight_model::guild::invite::Invite;
use twilight_model::guild::{Guild, Member};
use twilight_model::id::marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker};
use twilight_model::id::Id;

/// REST failure: transport/validation vs Discord API error vs body decode.
#[derive(Debug, Error)]
pub enum RestError {
    #[error("discord request failed: {0}")]
    Twilight(#[from] TwilightError),
    #[error("discord response body unreadable: {0}")]
    Body(#[from] twilight_http::response::DeserializeBodyError),
}

/// Paced twilight client with legacy retry semantics.
#[derive(Debug, Clone)]
pub struct RestClient {
    inner: Arc<RestInner>,
}

#[derive(Debug)]
struct RestInner {
    client: Client,
    min_interval: Duration,
    last_at: tokio::sync::Mutex<std::time::Instant>,
    requests: AtomicU64,
}

impl RestClient {
    /// Build against real Discord.
    #[must_use]
    pub fn new(token: String) -> Self {
        Self::with_proxy(token, None)
    }

    /// Build with an optional API-host override (mock verification).
    /// Accepts a full origin (`http://127.0.0.1:PORT`) or bare host:port:
    /// twilight's proxy takes the HOST only (`http://{host}/api/v10/...`),
    /// so any scheme prefix is stripped here.
    #[must_use]
    pub fn with_proxy(token: String, proxy_url: Option<String>) -> Self {
        let mut builder = Client::builder().token(token);
        if let Some(url) = proxy_url {
            let host = url
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .trim_end_matches('/');
            builder = builder.proxy(host.to_owned(), true);
        }
        Self {
            inner: Arc::new(RestInner {
                client: builder.build(),
                min_interval: Duration::from_millis(110),
                last_at: tokio::sync::Mutex::new(
                    std::time::Instant::now() - Duration::from_secs(60),
                ),
                requests: AtomicU64::new(0),
            }),
        }
    }

    /// Requests made so far: the run's own Discord cost report.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.inner.requests.load(Ordering::Relaxed)
    }

    async fn pace(&self) {
        let mut last = self.inner.last_at.lock().await;
        let earliest = *last + self.inner.min_interval;
        let now = std::time::Instant::now();
        if earliest > now {
            tokio::time::sleep(earliest - now).await;
        }
        *last = std::time::Instant::now();
    }

    // twilight surfaces the Discord `retry-after` via the ratelimiter, which
    // the tools bypass for one-shot pacing; 429s wait 1s + 250ms.

    /// Classify a request error: `RetryKind::GiveUpNone` for 403/404 and
    /// 5xx past the attempt budget, `RetryKind::Sleep` to retry after the
    /// delay, `RetryKind::Fail` for anything else.
    fn classify(&self, kind: &ErrorType, attempt: u32) -> RetryKind {
        match kind {
            ErrorType::Response { status, .. } => {
                let code = status.get();
                if code == 403 || code == 404 {
                    RetryKind::GiveUpNone
                } else if code == 429 {
                    RetryKind::Sleep(Duration::from_millis(1250))
                } else if code >= 500 {
                    if attempt >= 4 {
                        RetryKind::GiveUpNone
                    } else {
                        RetryKind::Sleep(backoff_duration(attempt))
                    }
                } else {
                    RetryKind::Fail
                }
            }
            _ => {
                if attempt >= 4 {
                    RetryKind::Fail
                } else {
                    RetryKind::Sleep(backoff_duration(attempt))
                }
            }
        }
    }

    /// Run one list request with legacy retry semantics. `make` builds the
    /// twilight request builder each attempt (builders are consumed by
    /// `IntoFuture`, so they cannot be reused across retries).
    async fn exec_list<T, F, Fut>(&self, make: F) -> Result<Option<Vec<T>>, RestError>
    where
        // `Unpin` is satisfied by every concrete twilight model; the bound
        // exists because `ModelFuture` awaits the deserialized value by value.
        T: serde::de::DeserializeOwned + Unpin,
        F: Fn() -> Fut,
        Fut: std::future::Future<
            Output = Result<
                twilight_http::Response<twilight_http::response::marker::ListBody<T>>,
                TwilightError,
            >,
        >,
    {
        let mut attempt: u32 = 0;
        loop {
            self.pace().await;
            self.inner.requests.fetch_add(1, Ordering::Relaxed);
            match make().await {
                Ok(resp) => return Ok(Some(resp.models().await?)),
                Err(e) => match self.classify(e.kind(), attempt) {
                    RetryKind::Sleep(delay) => {
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                    }
                    RetryKind::GiveUpNone => return Ok(None),
                    RetryKind::Fail => return Err(RestError::Twilight(e)),
                },
            }
        }
    }

    /// Run one single-model request with the same retry semantics.
    async fn exec_one<T, F, Fut>(&self, make: F) -> Result<Option<T>, RestError>
    where
        T: serde::de::DeserializeOwned + Unpin,
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<twilight_http::Response<T>, TwilightError>>,
    {
        let mut attempt: u32 = 0;
        loop {
            self.pace().await;
            self.inner.requests.fetch_add(1, Ordering::Relaxed);
            match make().await {
                Ok(resp) => return Ok(Some(resp.model().await?)),
                Err(e) => match self.classify(e.kind(), attempt) {
                    RetryKind::Sleep(delay) => {
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                    }
                    RetryKind::GiveUpNone => return Ok(None),
                    RetryKind::Fail => return Err(RestError::Twilight(e)),
                },
            }
        }
    }

    /// Page a guild's full member list (`joined_at` is Discord's own record).
    /// Returns `None` when a page is unreadable (strict callers treat that
    /// as failure, not as end-of-list — legacy `fetchAllMembersStrict`).
    pub async fn fetch_all_members(
        &self,
        guild_id: Id<GuildMarker>,
    ) -> Result<Option<Vec<Member>>, RestError> {
        let mut out = Vec::new();
        let mut after: Option<Id<UserMarker>> = None;
        loop {
            let page: Option<Vec<Member>> = self
                .exec_list(|| {
                    let client = &self.inner.client;
                    let mut req = client.guild_members(guild_id).limit(1000);
                    if let Some(a) = after {
                        req = req.after(a);
                    }
                    async move { req.await }
                })
                .await?;
            let Some(batch) = page else {
                return Ok(None);
            };
            if batch.is_empty() {
                break;
            }
            let full = batch.len() >= 1000;
            let last = batch.last().map(|m| m.user.id);
            out.extend(batch);
            match (last, full) {
                (Some(id), true) => after = Some(id),
                _ => break,
            }
        }
        Ok(Some(out))
    }

    /// Guild invites (attribution baseline / capture window counters).
    pub async fn guild_invites(
        &self,
        guild_id: Id<GuildMarker>,
    ) -> Result<Option<Vec<Invite>>, RestError> {
        self.exec_list(|| {
            let client = &self.inner.client;
            async move { client.guild_invites(guild_id).await }
        })
        .await
    }

    /// Guild channels (backfill candidate scan / message-ladder targets).
    pub async fn guild_channels(
        &self,
        guild_id: Id<GuildMarker>,
    ) -> Result<Option<Vec<Channel>>, RestError> {
        self.exec_list(|| {
            let client = &self.inner.client;
            async move { client.guild_channels(guild_id).await }
        })
        .await
    }

    /// Guild fetch (vanity-URL presence for attribution).
    pub async fn guild(&self, guild_id: Id<GuildMarker>) -> Result<Option<Guild>, RestError> {
        self.exec_one(|| {
            let client = &self.inner.client;
            async move { client.guild(guild_id).await }
        })
        .await
    }

    /// Active threads in a guild (forum posts live in threads).
    pub async fn active_threads(
        &self,
        guild_id: Id<GuildMarker>,
    ) -> Result<Option<twilight_model::channel::thread::ThreadsListing>, RestError> {
        self.exec_one(|| {
            let client = &self.inner.client;
            async move { client.active_threads(guild_id).await }
        })
        .await
    }

    /// Public archived threads of a channel (forum history).
    pub async fn public_archived_threads(
        &self,
        channel_id: Id<ChannelMarker>,
    ) -> Result<Option<twilight_model::channel::thread::ThreadsListing>, RestError> {
        self.exec_one(|| {
            let client = &self.inner.client;
            async move { client.public_archived_threads(channel_id).limit(100).await }
        })
        .await
    }

    /// Walk a channel's history newest-first (legacy `scanChannel`).
    /// `stop_before`: stop once older than this timestamp (millis). The
    /// caller converts ISO bounds to millis via [`iso_to_millis`].
    /// `max_pages` is a hard cost ceiling — hitting it reports `truncated`.
    pub async fn scan_channel(
        &self,
        channel_id: Id<ChannelMarker>,
        max_pages: usize,
        stop_before_ms: Option<i64>,
    ) -> Result<ScanPage, RestError> {
        let mut messages: Vec<Message> = Vec::new();
        let mut before: Option<Id<MessageMarker>> = None;
        let mut pages = 0usize;
        let mut truncated = false;

        loop {
            if pages >= max_pages {
                truncated = true;
                break;
            }
            let batch: Option<Vec<Message>> = match before {
                Some(b) => {
                    self.exec_list(|| {
                        let client = &self.inner.client;
                        async move {
                            client
                                .channel_messages(channel_id)
                                .limit(100)
                                .before(b)
                                .await
                        }
                    })
                    .await?
                }
                None => {
                    self.exec_list(|| {
                        let client = &self.inner.client;
                        async move { client.channel_messages(channel_id).limit(100).await }
                    })
                    .await?
                }
            };
            let Some(batch) = batch else {
                break;
            };
            if batch.is_empty() {
                break;
            }
            pages += 1;
            let short = batch.len() < 100;
            before = batch.last().map(|m| m.id);
            let past_stop = stop_before_ms.is_some_and(|bound| {
                batch
                    .last()
                    .is_some_and(|m| timestamp_ms(&m.timestamp) < bound)
            });
            messages.extend(batch);
            if short || past_stop {
                break;
            }
        }

        let scanned_back_to = messages.last().map(|m| m.timestamp.iso_8601().to_string());
        Ok(ScanPage {
            messages,
            scanned_back_to,
            truncated,
        })
    }
}

/// Retry outcome for one failed attempt.
enum RetryKind {
    Sleep(Duration),
    GiveUpNone,
    Fail,
}

fn backoff_duration(attempt: u32) -> Duration {
    Duration::from_millis(500u64.saturating_mul(1 << attempt.min(10)))
}

/// ISO-8601 → epoch millis for scan bounds.
#[must_use]
pub fn iso_to_millis(s: &str) -> Option<i64> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// twilight `Timestamp` → epoch millis.
#[must_use]
pub fn timestamp_ms(ts: &twilight_model::util::datetime::Timestamp) -> i64 {
    ts.as_micros() / 1000
}

/// One channel walk outcome (legacy `ScanResult`).
#[derive(Debug, Clone)]
pub struct ScanPage {
    pub messages: Vec<Message>,
    pub scanned_back_to: Option<String>,
    pub truncated: bool,
}
