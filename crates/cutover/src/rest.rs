//! Discord REST seam for the cutover tools.
//!
//! Ports `src/discord/rest.ts` onto twilight-http. Semantics preserved: a
//! 110ms pacing floor between requests (Discord allows 50/s globally; the
//! tools use far less), 403/404 are unreadable (not an abort), 429 backs
//! off, 5xx retries with exponential backoff (four retries). History scans
//! retain partial pages and an explicit completion reason; other callers
//! keep their legacy `None` on unreadable/exhausted pages.
//! `requests` counts calls so each run reports its
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
    /// A full member page did not advance the `after` cursor (repeated or
    /// out-of-order page): traversal cannot be proven complete.
    #[error("member pagination stalled: page did not advance the cursor")]
    MemberCursorStalled,
    /// A page/member ceiling was hit while Discord still reported full pages.
    #[error("member pagination exceeded ceiling ({pages} pages, {members} members)")]
    MemberCeilingExceeded { pages: u32, members: usize },
}

/// Default ceiling on member pages per traversal (1000 members/page, so
/// 500k members: far beyond any TWO guild yet a hard memory/request bound).
pub const MAX_MEMBER_PAGES: u32 = 500;
/// Default ceiling on accumulated members per traversal.
pub const MAX_MEMBERS: usize = 500_000;

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

    // Twilight handles 429 retries internally via its default ratelimiter.
    // Preserve the legacy fallback branch below without changing that behavior.
    // https://docs.rs/twilight-http/0.17.1/twilight_http/response/struct.ResponseFuture.html#rate-limits

    /// Classify a request error without confusing an unreadable/exhausted
    /// page with a successfully read empty page.
    fn classify(&self, kind: &ErrorType, attempt: u32) -> RetryKind {
        match kind {
            ErrorType::Response { status, .. } => {
                let code = status.get();
                if code == 403 || code == 404 {
                    RetryKind::GiveUp(ScanCompletion::Unreadable)
                } else if code == 429 {
                    RetryKind::Sleep(Duration::from_millis(1250))
                } else if code >= 500 {
                    if attempt >= 4 {
                        RetryKind::GiveUp(ScanCompletion::RetryExhausted)
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
    async fn exec_list<T, F, Fut>(&self, make: F) -> Result<ListPage<T>, RestError>
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
                Ok(resp) => return Ok(ListPage::Read(resp.models().await?)),
                Err(e) => match self.classify(e.kind(), attempt) {
                    RetryKind::Sleep(delay) => {
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                    }
                    RetryKind::GiveUp(reason) => return Ok(ListPage::Interrupted(reason)),
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
                    RetryKind::GiveUp(_) => return Ok(None),
                    RetryKind::Fail => return Err(RestError::Twilight(e)),
                },
            }
        }
    }

    /// Page a guild's full member list (`joined_at` is Discord's own record).
    /// Returns `None` when a page is unreadable (strict callers treat that
    /// as failure, not as end-of-list — legacy `fetchAllMembersStrict`).
    /// Traversal is bounded by [`MAX_MEMBER_PAGES`] / [`MAX_MEMBERS`]; a
    /// non-advancing cursor or a ceiling hit is an `Err`, never a partial
    /// roster presented as complete.
    pub async fn fetch_all_members(
        &self,
        guild_id: Id<GuildMarker>,
    ) -> Result<Option<Vec<Member>>, RestError> {
        self.fetch_all_members_bounded(guild_id, MAX_MEMBER_PAGES, MAX_MEMBERS)
            .await
    }

    /// [`Self::fetch_all_members`] with explicit ceilings (tests, tuning).
    pub async fn fetch_all_members_bounded(
        &self,
        guild_id: Id<GuildMarker>,
        max_pages: u32,
        max_members: usize,
    ) -> Result<Option<Vec<Member>>, RestError> {
        let mut out = Vec::new();
        let mut after: Option<Id<UserMarker>> = None;
        let mut pages: u32 = 0;
        loop {
            if pages >= max_pages || out.len() >= max_members {
                return Err(RestError::MemberCeilingExceeded {
                    pages,
                    members: out.len(),
                });
            }
            let page: ListPage<Member> = self
                .exec_list(|| {
                    let client = &self.inner.client;
                    let mut req = client.guild_members(guild_id).limit(1000);
                    if let Some(a) = after {
                        req = req.after(a);
                    }
                    async move { req.await }
                })
                .await?;
            let ListPage::Read(batch) = page else {
                return Ok(None);
            };
            pages += 1;
            if batch.is_empty() {
                break;
            }
            let full = batch.len() >= 1000;
            let last = batch.last().map(|m| m.user.id);
            out.extend(batch);
            match (last, full) {
                (Some(id), true) => {
                    if after.is_some_and(|prev| id <= prev) {
                        return Err(RestError::MemberCursorStalled);
                    }
                    after = Some(id);
                }
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
        .map(ListPage::into_option)
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
        .map(ListPage::into_option)
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
        let completion = loop {
            if pages >= max_pages {
                break ScanCompletion::PageCap;
            }
            let page = self
                .exec_list(|| {
                    let req = self.inner.client.channel_messages(channel_id);
                    async move {
                        match before {
                            Some(b) => req.limit(100).before(b).await,
                            None => req.limit(100).await,
                        }
                    }
                })
                .await;
            let batch: Vec<Message> = match page {
                Ok(ListPage::Read(batch)) => batch,
                Ok(ListPage::Interrupted(reason)) => break reason,
                Err(RestError::Body(_)) => break ScanCompletion::InvalidResponse,
                Err(RestError::Twilight(_)) => break ScanCompletion::RequestFailed,
            };
            if batch.is_empty() {
                break ScanCompletion::EndOfHistory;
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
            // A short page proves history ended, even if it also crosses the bound.
            if short {
                break ScanCompletion::EndOfHistory;
            }
            if past_stop {
                break ScanCompletion::TimeBoundary;
            }
        };

        let scanned_back_to = messages.last().map(|m| m.timestamp.iso_8601().to_string());
        Ok(ScanPage {
            messages,
            scanned_back_to,
            truncated: completion == ScanCompletion::PageCap,
            completion,
        })
    }
}

/// Retry outcome for one failed attempt.
enum RetryKind {
    Sleep(Duration),
    GiveUp(ScanCompletion),
    Fail,
}

/// A failed list page is not an empty list. Non-history callers retain their
/// legacy optional result at the boundary, not inside the retry executor.
enum ListPage<T> {
    Read(Vec<T>),
    Interrupted(ScanCompletion),
}

impl<T> ListPage<T> {
    fn into_option(self) -> Option<Vec<T>> {
        match self {
            Self::Read(batch) => Some(batch),
            Self::Interrupted(_) => None,
        }
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

/// Why a history walk stopped. A time boundary completes the requested window,
/// not necessarily the entire history; a page cap or interruption is incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScanCompletion {
    EndOfHistory,
    TimeBoundary,
    PageCap,
    Unreadable,
    RetryExhausted,
    RequestFailed,
    InvalidResponse,
}

impl ScanCompletion {
    #[must_use]
    pub fn interrupted(self) -> bool {
        matches!(
            self,
            Self::Unreadable | Self::RetryExhausted | Self::RequestFailed | Self::InvalidResponse
        )
    }
}

impl std::fmt::Display for ScanCompletion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::EndOfHistory => "end-of-history",
            Self::TimeBoundary => "time-boundary",
            Self::PageCap => "page-cap",
            Self::Unreadable => "unreadable",
            Self::RetryExhausted => "retry-exhausted",
            Self::RequestFailed => "request-failed",
            Self::InvalidResponse => "invalid-response",
        })
    }
}

/// One channel walk outcome (legacy `ScanResult` plus explicit completion).
#[derive(Debug, Clone)]
pub struct ScanPage {
    pub messages: Vec<Message>,
    pub scanned_back_to: Option<String>,
    /// Legacy page-cap flag only; interruptions are described by `completion`.
    pub truncated: bool,
    pub completion: ScanCompletion,
}
