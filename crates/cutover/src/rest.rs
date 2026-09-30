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
use twilight_http::error::Error as TwilightError;
use twilight_http::request::{Request, TryIntoRequest};
use twilight_http::Client;
use twilight_model::channel::message::Message;
use twilight_model::channel::Channel;
use twilight_model::guild::invite::Invite;
use twilight_model::guild::{Guild, Member};
use twilight_model::id::marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker};
use twilight_model::id::Id;
use two_bot_core::send_admission::{PgSendAdmission, SendAdmission};
use two_bot_discord::executor::HyperTransport;

/// REST failure: transport/validation vs Discord API error vs body decode.
#[derive(Debug, Error)]
pub enum RestError {
    #[error("discord request failed: {0}")]
    Twilight(#[from] TwilightError),
    #[error("discord response body unreadable: {0}")]
    Body(#[from] serde_json::Error),
    #[error("discord send refused: {0}")]
    Wire(String),
}

/// Paced twilight client with legacy retry semantics.
#[derive(Debug, Clone)]
pub struct RestClient {
    inner: Arc<RestInner>,
}

#[derive(Debug)]
struct RestInner {
    client: Client,
    transport: Result<HyperTransport, String>,
    min_interval: Duration,
    last_at: tokio::sync::Mutex<std::time::Instant>,
    requests: AtomicU64,
}

impl RestClient {
    /// Ungoverned compatibility constructor; live sends are refused. Runtime
    /// CLI callers use `from_env` to bind the durable authority.
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
        Self::build(token, proxy_url, None)
    }

    pub fn with_admission(
        token: String,
        proxy_url: Option<String>,
        admission: Arc<dyn SendAdmission>,
    ) -> Self {
        Self::build(token, proxy_url, Some(admission))
    }

    /// Administrative data targets may differ; admission must use the runtime's
    /// TWO_DATABASE_URL authority, never the backfill/capture target database.
    pub async fn from_env(token: String, proxy_url: Option<String>) -> Result<Self, RestError> {
        let url = std::env::var("TWO_DATABASE_URL").map_err(|_| {
            RestError::Wire("TWO_DATABASE_URL admission authority required".to_owned())
        })?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .map_err(|_| RestError::Wire("admission authority unavailable".to_owned()))?;
        let gate =
            PgSendAdmission::new(pool, &token).map_err(|e| RestError::Wire(e.to_string()))?;
        Ok(Self::with_admission(token, proxy_url, Arc::new(gate)))
    }

    fn build(
        token: String,
        proxy_url: Option<String>,
        admission: Option<Arc<dyn SendAdmission>>,
    ) -> Self {
        let transport = match admission {
            Some(gate) => HyperTransport::with_admission(token.clone(), proxy_url.clone(), gate),
            None => HyperTransport::with_proxy(token.clone(), proxy_url.clone()),
        };
        let mut builder = Client::builder().token(token).ratelimiter(None);
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
                transport,
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

    /// Twilight is only a request factory: ResponseFuture hides 429 resends.
    /// Every actual attempt instead passes through the governed raw transport.
    async fn exec_one<T, F>(&self, make: F) -> Result<Option<T>, RestError>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> Result<Request, TwilightError>,
    {
        let transport = self
            .inner
            .transport
            .as_ref()
            .map_err(|e| RestError::Wire(e.clone()))?;
        for attempt in 0..=4 {
            self.pace().await;
            let request = make()?;
            self.inner.requests.fetch_add(1, Ordering::Relaxed);
            let res =
                tokio::time::timeout(Duration::from_secs(30), transport.send_request(&request))
                    .await
                    .map_err(|_| RestError::Wire("request timed out; lane held".to_owned()))?
                    .map_err(RestError::Wire)?;
            match res.status {
                200..=299 => return Ok(Some(serde_json::from_slice(&res.body)?)),
                403 | 404 => return Ok(None),
                429 if attempt < 4 => {
                    tokio::time::sleep(Duration::from_millis(res.retry_after_wait_ms())).await
                }
                500..=599 if attempt < 4 => tokio::time::sleep(backoff_duration(attempt)).await,
                500..=599 => return Ok(None),
                _ => return Err(RestError::Wire(format!("HTTP {}", res.status))),
            }
        }
        unreachable!("last attempt always returns")
    }

    async fn exec_list<T, F>(&self, make: F) -> Result<Option<Vec<T>>, RestError>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> Result<Request, TwilightError>,
    {
        self.exec_one(make).await
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
                    req.try_into_request()
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
            client.guild_invites(guild_id).try_into_request()
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
            client.guild_channels(guild_id).try_into_request()
        })
        .await
    }

    /// Guild fetch (vanity-URL presence for attribution).
    pub async fn guild(&self, guild_id: Id<GuildMarker>) -> Result<Option<Guild>, RestError> {
        self.exec_one(|| {
            let client = &self.inner.client;
            client.guild(guild_id).try_into_request()
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
            client.active_threads(guild_id).try_into_request()
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
            client
                .public_archived_threads(channel_id)
                .limit(100)
                .try_into_request()
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
                        client
                            .channel_messages(channel_id)
                            .limit(100)
                            .before(b)
                            .try_into_request()
                    })
                    .await?
                }
                None => {
                    self.exec_list(|| {
                        let client = &self.inner.client;
                        client
                            .channel_messages(channel_id)
                            .limit(100)
                            .try_into_request()
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
