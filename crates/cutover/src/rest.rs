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

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use twilight_http::error::Error as TwilightError;
use twilight_http::request::{AuditLogReason, Request, TryIntoRequest};
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

/// What a channel delete proved about the channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// 2xx: this call deleted the channel.
    Deleted,
    /// 404: the channel was already gone; nothing was deleted.
    AlreadyGone,
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
        let options = two_bot_core::database_url::connect_options(&url)
            .map_err(|_| RestError::Wire("admission authority unavailable".to_owned()))?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
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
    /// Every actual attempt instead passes through the governed raw transport,
    /// preserving the legacy classification — 403/404 give up as unreadable,
    /// 429 parks per the response's own retry-after, 5xx backs off and then
    /// gives up as retry-exhausted, anything else fails.
    async fn exec_list<T, F>(&self, make: F) -> Result<ListPage<T>, RestError>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> Result<Request, TwilightError>,
    {
        let transport = self
            .inner
            .transport
            .as_ref()
            .map_err(|e| RestError::Wire(e.clone()))?;
        let mut attempt: u32 = 0;
        loop {
            self.pace().await;
            let request = make()?;
            self.inner.requests.fetch_add(1, Ordering::Relaxed);
            let (res, _) =
                tokio::time::timeout(Duration::from_secs(30), transport.send_request(&request))
                    .await
                    .map_err(|_| RestError::Wire("request timed out; lane held".to_owned()))?
                    .map_err(RestError::Wire)?;
            match res.status {
                200..=299 => return Ok(ListPage::Read(serde_json::from_slice(&res.body)?)),
                403 | 404 => return Ok(ListPage::Interrupted(ScanCompletion::Unreadable)),
                429 if attempt < 4 => {
                    tokio::time::sleep(Duration::from_millis(res.retry_after_wait_ms())).await
                }
                500..=599 if attempt < 4 => tokio::time::sleep(backoff_duration(attempt)).await,
                500..=599 => return Ok(ListPage::Interrupted(ScanCompletion::RetryExhausted)),
                _ => return Err(RestError::Wire(format!("HTTP {}", res.status))),
            }
            attempt += 1;
        }
    }

    /// Run one single-model request with the same transport-governed retry
    /// semantics; a give-up page reads as `None` (legacy unreadable/absent).
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
            let (res, _) =
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
                    req.try_into_request()
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
            client.guild_invites(guild_id).try_into_request()
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
            client.guild_channels(guild_id).try_into_request()
        })
        .await
        .map(ListPage::into_option)
    }

    /// Delete one channel (ghost-cleanup execute path), carrying the
    /// operator's `reason` in the Discord audit-log header. Same governed
    /// retry semantics as the readers, minus body decode (DELETE answers
    /// 204 with no body): 2xx is deleted, 404 is already-gone success (a
    /// duplicate delete is success), 429 backs off on the response's own
    /// retry-after, 5xx retries with backoff. A 403 is a hard failure —
    /// never proof of deletion, so the row stays tracked for the next run.
    pub async fn delete_channel(
        &self,
        channel_id: Id<ChannelMarker>,
        reason: &str,
    ) -> Result<DeleteOutcome, RestError> {
        let transport = self
            .inner
            .transport
            .as_ref()
            .map_err(|e| RestError::Wire(e.clone()))?;
        for attempt in 0..=4 {
            self.pace().await;
            let request = self
                .inner
                .client
                .delete_channel(channel_id)
                .reason(reason)
                .try_into_request()?;
            self.inner.requests.fetch_add(1, Ordering::Relaxed);
            let (res, _) =
                tokio::time::timeout(Duration::from_secs(30), transport.send_request(&request))
                    .await
                    .map_err(|_| RestError::Wire("request timed out; lane held".to_owned()))?
                    .map_err(RestError::Wire)?;
            match res.status {
                200..=299 => return Ok(DeleteOutcome::Deleted),
                404 => return Ok(DeleteOutcome::AlreadyGone),
                403 => return Err(RestError::Wire("HTTP 403".to_owned())),
                429 if attempt < 4 => {
                    tokio::time::sleep(Duration::from_millis(res.retry_after_wait_ms())).await;
                }
                500..=599 if attempt < 4 => tokio::time::sleep(backoff_duration(attempt)).await,
                500..=599 => return Err(RestError::Wire("HTTP 5xx retry exhausted".to_owned())),
                _ => return Err(RestError::Wire(format!("HTTP {}", res.status))),
            }
        }
        unreachable!("last attempt always returns")
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
    ///
    /// Follows the listing's own continuation the way Discord documents it:
    /// each page carries `has_more`, and the next page passes `before` with
    /// the oldest thread's `archive_timestamp` (listings run newest-first).
    /// Returns every thread exactly once by snowflake; an unreadable page
    /// reports `Ok(None)` (legacy: an unreadable channel is a normal
    /// condition), while a repeated or non-progressing continuation and a
    /// spent page budget report [`ArchivedThreadsOutcome::Incomplete`] with
    /// the threads seen so far — never an infinite loop or a silently
    /// complete result.
    pub async fn public_archived_threads(
        &self,
        channel_id: Id<ChannelMarker>,
    ) -> Result<Option<twilight_model::channel::thread::ThreadsListing>, RestError> {
        Ok(self
            .list_all_archived_threads(channel_id, DEFAULT_ARCHIVED_THREAD_PAGES)
            .await?
            .map(|outcome| outcome.into_listing()))
    }

    /// Bounded archived-thread walk with explicit completion evidence.
    /// `max_pages` is the same hard cost ceiling [`Self::scan_channel`] uses:
    /// hitting it reports `incomplete` rather than dropping older threads
    /// silently. Reports `Ok(None)` when the first page is unreadable, so
    /// the compat helper keeps the legacy no-listing contract; a later
    /// unreadable page ends the walk with the threads seen so far.
    pub async fn list_all_archived_threads(
        &self,
        channel_id: Id<ChannelMarker>,
        max_pages: usize,
    ) -> Result<Option<ArchivedThreadsOutcome>, RestError> {
        let mut threads: Vec<Channel> = Vec::new();
        let mut seen: HashSet<u64> = HashSet::new();
        let mut before: Option<String> = None;
        let mut pages = 0usize;
        let mut last_cursor: Option<String> = None;

        loop {
            if pages >= max_pages {
                return Ok(Some(ArchivedThreadsOutcome::Incomplete {
                    threads,
                    reason: ArchiveIncompleteReason::PageBudget,
                }));
            }
            let page: Option<twilight_model::channel::thread::ThreadsListing> = self
                .exec_one(|| {
                    let client = &self.inner.client;
                    match before.as_deref() {
                        Some(cursor) => client
                            .public_archived_threads(channel_id)
                            .limit(100)
                            .before(cursor)
                            .try_into_request(),
                        None => client
                            .public_archived_threads(channel_id)
                            .limit(100)
                            .try_into_request(),
                    }
                })
                .await?;
            let Some(listing) = page else {
                // Nothing was readable at all: stay `None`, exactly like the
                // legacy single-page consumer. A later unreadable page ends
                // the walk with the threads seen so far.
                if threads.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(ArchivedThreadsOutcome::Complete { threads }));
            };
            if listing.threads.is_empty() {
                return Ok(Some(ArchivedThreadsOutcome::Complete { threads }));
            }
            pages += 1;
            for thread in &listing.threads {
                if seen.insert(thread.id.get()) {
                    threads.push(thread.clone());
                }
            }
            let has_more = listing.has_more.unwrap_or(false);
            if !has_more {
                return Ok(Some(ArchivedThreadsOutcome::Complete { threads }));
            }
            let Some(cursor) = oldest_archive_timestamp(&listing) else {
                // `has_more` with no usable cursor cannot advance: report the
                // partial walk instead of re-requesting the same page forever.
                return Ok(Some(ArchivedThreadsOutcome::Incomplete {
                    threads,
                    reason: ArchiveIncompleteReason::NoProgress,
                }));
            };
            if last_cursor.as_deref() == Some(cursor.as_str()) {
                return Ok(Some(ArchivedThreadsOutcome::Incomplete {
                    threads,
                    reason: ArchiveIncompleteReason::NoProgress,
                }));
            }
            last_cursor = Some(cursor.clone());
            before = Some(cursor);
        }
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
                    let client = &self.inner.client;
                    match before {
                        Some(b) => client
                            .channel_messages(channel_id)
                            .limit(100)
                            .before(b)
                            .try_into_request(),
                        None => client
                            .channel_messages(channel_id)
                            .limit(100)
                            .try_into_request(),
                    }
                })
                .await;
            let batch: Vec<Message> = match page {
                Ok(ListPage::Read(batch)) => batch,
                Ok(ListPage::Interrupted(reason)) => break reason,
                Err(RestError::Body(_)) => break ScanCompletion::InvalidResponse,
                Err(RestError::Twilight(_)) => break ScanCompletion::RequestFailed,
                Err(RestError::Wire(_)) => break ScanCompletion::RequestFailed,
                // Member-pagination errors never come from `exec_list`;
                // propagate rather than mislabel a scan completion.
                Err(
                    e @ (RestError::MemberCursorStalled | RestError::MemberCeilingExceeded { .. }),
                ) => return Err(e),
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

/// Default page budget for archived-thread discovery: a 130-thread forum
/// needs two limit(100) pages, so two pages complete the common case while
/// a stuck continuation still terminates quickly.
pub const DEFAULT_ARCHIVED_THREAD_PAGES: usize = 10;

/// Why archived-thread discovery stopped with threads potentially unseen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveIncompleteReason {
    /// The page budget ran out before `has_more` cleared.
    PageBudget,
    /// The next cursor repeated or the listing offered no cursor to follow.
    NoProgress,
}

/// Bounded archived-thread discovery outcome: either every page through
/// `has_more == false`, or the partial walk plus why it stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchivedThreadsOutcome {
    Complete {
        threads: Vec<Channel>,
    },
    Incomplete {
        threads: Vec<Channel>,
        reason: ArchiveIncompleteReason,
    },
}

impl ArchivedThreadsOutcome {
    /// Threads discovered so far, complete or not.
    #[must_use]
    pub fn threads(&self) -> &[Channel] {
        match self {
            Self::Complete { threads } | Self::Incomplete { threads, .. } => threads,
        }
    }

    /// True only when `has_more` cleared and every continuation progressed.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete { .. })
    }

    /// Back-compat single listing for the legacy one-page call site: the
    /// threads seen so far. `list_all_archived_threads` returns `None`
    /// instead of calling this when nothing was readable at all.
    #[must_use]
    pub fn into_listing(self) -> twilight_model::channel::thread::ThreadsListing {
        let (threads, complete) = match self {
            Self::Complete { threads } => (threads, true),
            Self::Incomplete { threads, .. } => (threads, false),
        };
        twilight_model::channel::thread::ThreadsListing {
            has_more: Some(!complete),
            members: Vec::new(),
            threads,
        }
    }
}

/// Oldest `archive_timestamp` in a listing: the `before` cursor for the next
/// page. Listings run newest-first, so the last thread carries the cursor.
fn oldest_archive_timestamp(
    listing: &twilight_model::channel::thread::ThreadsListing,
) -> Option<String> {
    listing
        .threads
        .iter()
        .filter_map(|t| t.thread_metadata.as_ref())
        .map(|m| m.archive_timestamp.iso_8601().to_string())
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use twilight_model::channel::thread::{AutoArchiveDuration, ThreadMetadata, ThreadsListing};
    use twilight_model::channel::{Channel, ChannelType};

    const FORUM: u64 = 7_001;

    /// One scripted response: either a listing page or a raw status body.
    enum Scripted {
        Page(ThreadsListing),
        Status(u16, String),
    }

    /// Scripted listing responses consumed in request order; `recorded`
    /// captures the `before` query value of each request (decoded, `None`
    /// for the first page) so tests can assert the continuation chain.
    struct ArchiveMock {
        recorded: Arc<Mutex<Vec<Option<String>>>>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl ArchiveMock {
        async fn start(pages: Vec<ThreadsListing>) -> (Self, String) {
            Self::start_script(pages.into_iter().map(Scripted::Page).collect()).await
        }

        async fn start_script(script: Vec<Scripted>) -> (Self, String) {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock listen");
            let origin = format!("http://{}", listener.local_addr().expect("addr"));
            let recorded = Arc::new(Mutex::new(Vec::new()));
            let script = Arc::new(Mutex::new(script.into_iter().collect::<VecDeque<_>>()));
            let (rec, scr) = (recorded.clone(), script.clone());
            let handle = tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let (rec, scr) = (rec.clone(), scr.clone());
                    tokio::spawn(async move {
                        let Some(path) = read_get(&mut stream).await else {
                            return;
                        };
                        rec.lock()
                            .expect("recorded")
                            .push(decode_before_query(&path));
                        let next =
                            scr.lock()
                                .expect("script")
                                .pop_front()
                                .unwrap_or(Scripted::Page(ThreadsListing {
                                    has_more: Some(false),
                                    members: Vec::new(),
                                    threads: Vec::new(),
                                }));
                        let (status, reason, body) = match next {
                            Scripted::Page(listing) => (
                                200,
                                "OK",
                                serde_json::to_string(&listing).expect("fixture serializes"),
                            ),
                            Scripted::Status(status, body) => (status, reason_for(status), body),
                        };
                        let head = format!(
                            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream.write_all(body.as_bytes()).await;
                    });
                }
            });
            (Self { recorded, handle }, origin)
        }

        fn befores(&self) -> Vec<Option<String>> {
            self.recorded.lock().expect("recorded").clone()
        }

        fn shutdown(self) {
            self.handle.abort();
        }
    }

    fn reason_for(status: u16) -> &'static str {
        match status {
            200 => "OK",
            403 => "Forbidden",
            404 => "Not Found",
            _ => "Internal Server Error",
        }
    }

    async fn read_get(stream: &mut TcpStream) -> Option<String> {
        let mut buf = Vec::with_capacity(1024);
        let header_end = loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
            if buf.len() > 64 * 1024 {
                return None;
            }
            if stream.read_buf(&mut buf).await.ok()? == 0 {
                return None;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let request_line = head.split("\r\n").next()?;
        Some(request_line.split(' ').nth(1)?.to_owned())
    }

    /// Decode the `before` query value the client percent-encodes (`+` stays
    /// literal, `%XX` decodes), or `None` when the request has no cursor yet.
    fn decode_before_query(path: &str) -> Option<String> {
        let query = path.split_once('?')?.1;
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=')?;
            if k == "before" {
                return Some(percent_decode(v));
            }
        }
        None
    }

    fn percent_decode(s: &str) -> String {
        let mut out = Vec::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let hex_pair = bytes[i] == b'%'
                && i + 2 < bytes.len()
                && hex_val(bytes[i + 1]).is_some()
                && hex_val(bytes[i + 2]).is_some();
            if hex_pair {
                let h = hex_val(bytes[i + 1]).expect("checked above");
                let l = hex_val(bytes[i + 2]).expect("checked above");
                out.push(h << 4 | l);
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).expect("cursor is ASCII ISO-8601")
    }

    fn hex_val(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }

    /// One synthetic archived thread: snowflake `id`, archived at
    /// `2024-01-01T00:{mm}:{ss}.000000+00:00`.
    fn thread(id: u64, minute: u64, second: u64) -> Channel {
        let ts = twilight_model::util::Timestamp::from_str(&format!(
            "2024-01-01T00:{minute:02}:{second:02}.000000+00:00"
        ))
        .expect("fixture timestamp");
        Channel {
            application_id: None,
            applied_tags: None,
            available_tags: None,
            bitrate: None,
            default_auto_archive_duration: None,
            default_forum_layout: None,
            default_reaction_emoji: None,
            default_sort_order: None,
            default_thread_rate_limit_per_user: None,
            flags: None,
            guild_id: None,
            icon: None,
            id: Id::new(id),
            invitable: None,
            kind: ChannelType::PublicThread,
            last_message_id: None,
            last_pin_timestamp: None,
            managed: None,
            member: None,
            member_count: None,
            message_count: None,
            name: None,
            newly_created: None,
            nsfw: None,
            owner_id: None,
            parent_id: None,
            permission_overwrites: None,
            position: None,
            rate_limit_per_user: None,
            recipients: None,
            rtc_region: None,
            thread_metadata: Some(ThreadMetadata {
                archived: true,
                auto_archive_duration: AutoArchiveDuration::Day,
                archive_timestamp: ts,
                create_timestamp: None,
                invitable: None,
                locked: false,
            }),
            topic: None,
            user_limit: None,
            video_quality_mode: None,
        }
    }

    fn page(threads: Vec<Channel>, has_more: bool) -> ThreadsListing {
        ThreadsListing {
            has_more: Some(has_more),
            members: Vec::new(),
            threads,
        }
    }

    /// 130 newest-first threads (`id` 130 newest … 1 oldest, one per minute).
    fn forum_130() -> Vec<Channel> {
        (1u64..=130)
            .rev()
            .map(|id| thread(9_000_000_000_000_000_000 + id, id / 60, id % 60))
            .collect()
    }

    fn client_for(origin: &str) -> RestClient {
        RestClient::with_proxy("archived-test-token".to_owned(), Some(origin.to_owned()))
    }

    fn forum_id() -> Id<ChannelMarker> {
        Id::new(FORUM)
    }

    #[tokio::test]
    async fn discovers_all_130_threads_across_two_pages() {
        let all = forum_130();
        let cursor = all[99]
            .thread_metadata
            .as_ref()
            .expect("fixture metadata")
            .archive_timestamp
            .iso_8601()
            .to_string();
        let (mock, origin) = ArchiveMock::start(vec![
            page(all[..100].to_vec(), true),
            page(all[100..].to_vec(), false),
        ])
        .await;
        let rest = client_for(&origin);

        let outcome = rest
            .list_all_archived_threads(forum_id(), DEFAULT_ARCHIVED_THREAD_PAGES)
            .await
            .expect("walk succeeds")
            .expect("forum is readable");

        assert!(outcome.is_complete(), "two full pages complete");
        let threads = outcome.threads();
        assert_eq!(threads.len(), 130, "every thread discovered");
        let mut ids: Vec<u64> = threads.iter().map(|t| t.id.get()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 130, "each thread discovered exactly once");
        let befores = mock.befores();
        assert_eq!(befores.len(), 2, "one continuation request");
        assert_eq!(befores[0], None, "first page has no cursor");
        assert_eq!(
            befores[1],
            Some(cursor),
            "second page continues past page one"
        );
        mock.shutdown();
    }

    #[tokio::test]
    async fn empty_final_page_terminates_complete() {
        let (mock, origin) = ArchiveMock::start(vec![page(Vec::new(), false)]).await;
        let rest = client_for(&origin);

        let outcome = rest
            .list_all_archived_threads(forum_id(), DEFAULT_ARCHIVED_THREAD_PAGES)
            .await
            .expect("walk succeeds")
            .expect("forum is readable");

        assert!(outcome.is_complete());
        assert!(outcome.threads().is_empty());
        assert_eq!(mock.befores().len(), 1, "no continuation after empty page");
        mock.shutdown();
    }

    #[tokio::test]
    async fn cleared_has_more_terminates_complete() {
        let all = forum_130();
        let (mock, origin) = ArchiveMock::start(vec![page(all[..50].to_vec(), false)]).await;
        let rest = client_for(&origin);

        let outcome = rest
            .list_all_archived_threads(forum_id(), DEFAULT_ARCHIVED_THREAD_PAGES)
            .await
            .expect("walk succeeds")
            .expect("forum is readable");

        assert!(outcome.is_complete());
        assert_eq!(outcome.threads().len(), 50);
        assert_eq!(
            mock.befores().len(),
            1,
            "no continuation after has_more=false"
        );
        mock.shutdown();
    }

    #[tokio::test]
    async fn repeated_continuation_reports_incomplete() {
        let batch = forum_130()[..100].to_vec();
        let (mock, origin) =
            ArchiveMock::start(vec![page(batch.clone(), true), page(batch.clone(), true)]).await;
        let rest = client_for(&origin);

        let outcome = rest
            .list_all_archived_threads(forum_id(), DEFAULT_ARCHIVED_THREAD_PAGES)
            .await
            .expect("walk succeeds")
            .expect("forum is readable");

        assert!(!outcome.is_complete(), "repeating cursor is not complete");
        assert_eq!(
            outcome,
            ArchivedThreadsOutcome::Incomplete {
                threads: outcome.threads().to_vec(),
                reason: ArchiveIncompleteReason::NoProgress,
            }
        );
        assert_eq!(outcome.threads().len(), 100, "deduped by snowflake");
        assert_eq!(mock.befores().len(), 2, "stops instead of looping");
        mock.shutdown();
    }

    #[tokio::test]
    async fn page_budget_reports_incomplete() {
        let all = forum_130();
        let (mock, origin) = ArchiveMock::start(vec![
            page(all[..100].to_vec(), true),
            page(all[100..].to_vec(), true),
        ])
        .await;
        let rest = client_for(&origin);

        let outcome = rest
            .list_all_archived_threads(forum_id(), 1)
            .await
            .expect("walk succeeds");

        assert_eq!(
            outcome,
            Some(ArchivedThreadsOutcome::Incomplete {
                threads: all[..100].to_vec(),
                reason: ArchiveIncompleteReason::PageBudget,
            })
        );
        assert_eq!(mock.befores().len(), 1, "budget stops the walk");
        mock.shutdown();
    }

    #[tokio::test]
    async fn unreadable_second_page_ends_walk_with_threads_seen() {
        // First page readable with `has_more`, second page 403: the walk
        // ends `Complete` with page one's threads instead of failing.
        let all = forum_130();
        let (mock, origin) = ArchiveMock::start_script(vec![
            Scripted::Page(page(all[..100].to_vec(), true)),
            Scripted::Status(403, "{}".to_owned()),
        ])
        .await;
        let rest = client_for(&origin);

        let outcome = rest
            .list_all_archived_threads(forum_id(), DEFAULT_ARCHIVED_THREAD_PAGES)
            .await
            .expect("partial walk is not an error")
            .expect("first page was readable");

        assert!(outcome.is_complete());
        assert_eq!(outcome.threads().len(), 100);
        assert_eq!(mock.befores().len(), 2, "walked until unreadable");
        mock.shutdown();
    }

    #[tokio::test]
    async fn unreadable_forum_reports_no_listing_like_before() {
        // `exec_one` maps 403/404 to `None`; the legacy consumer sees exactly
        // what the old one-page call returned for an unreadable forum.
        let (mock, origin) = ArchiveMock::start_script(vec![
            Scripted::Status(403, "{}".to_owned()),
            Scripted::Status(403, "{}".to_owned()),
        ])
        .await;
        let rest = client_for(&origin);

        let outcome = rest
            .list_all_archived_threads(forum_id(), DEFAULT_ARCHIVED_THREAD_PAGES)
            .await
            .expect("unreadable forum is not an error");
        assert!(
            outcome.is_none(),
            "unreadable first page stays None like the legacy call"
        );
        assert_eq!(
            rest.public_archived_threads(forum_id())
                .await
                .expect("compat helper"),
            None,
            "compat helper preserves the legacy None contract"
        );
        assert_eq!(
            mock.befores().len(),
            2,
            "one discovery page plus one compat call"
        );
        mock.shutdown();
    }
}
